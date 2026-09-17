# Multi-account V2 — TODO

Fonte de requisitos: `multi-account-plan-v2.md`, o documento de handoff mantido
em paralelo a este arquivo — atualizar os dois a cada marco. Ele não vive neste
repositório; este arquivo é o resumo canônico do estado da implementação.
A implementação usa **um `whatsapp.db` compartilhado**; `AccountId` é o `device.id`
positivo desse banco. Não reintroduzir `accounts.json`, UUIDs, um DB por conta ou
migração de arquivo.

**PR aberto (draft):** `https://github.com/oxidezap/client/pull/172` (branch
`feat/multi-account`). CI estava vermelho por um conflito de ordenação de
migrations com o PR #171 (mergeado em `main` depois que esta branch foi
criada); corrigido e re-empurrado — ver a entrada "CI do PR #172 corrigido"
abaixo para o diagnóstico completo.

## Estado atual

- [x] Corrigir `AccountId` para `i32` opaco, validado (`> 0`) e serializável.
- [x] Preservar `AccountId(1)` como slot legado da instalação existente.
- [x] Abrir o backend WhatsApp por `device_id` através de `StoreRegistry`.
- [x] Adicionar foreign keys `device_id -> device(id) ON DELETE CASCADE` ao
      `chat-store` para chats, mensagens, reações, contatos, receipts e mídia.
- [x] Testar isolamento A/B no mesmo arquivo: remoção das rows de A preserva
      integralmente as rows de B.
- [x] Atualizar fixtures para respeitar a tabela pai `device` e validar o
      comportamento com o store público upstream.
- [x] Validações concluídas: `fmt`, `core`, `chat-store`, `session` isolado e
      `check` do `session`.
- [x] Investigar a sensibilidade de scheduling no teste completo de `session`:
      a execução paralela apresentou uma falha transitória, mas a suíte completa
      passou com `--test-threads=1`, e o teste também passa isolado.
      A invariável de sessão meio-aberta foi mantida; nenhuma asserção foi
      enfraquecida.
- [x] **WR-1 upstream mergeado.** `https://github.com/oxidezap/whatsapp-rust/pull/1505`,
      merged como `3a08e84e8d7a462f6f87eb98d28fe1ce889a653a` em `main`. Publica em
      `SqliteStore` (= `whatsapp_rust_sqlite_storage::SqliteStore`, reexportado):
      `list_devices()`, `create_sibling_device()`, `reset_device(id)`,
      `remove_device(id)`. Alocação de id via `AUTOINCREMENT` +
      `last_insert_rowid()` na mesma transação (livre de corrida; testes
      upstream cobrem 16 criações concorrentes). `reset_device` apaga e
      recria a row `device` com o mesmo id e chaves novas, em uma única
      `BEGIN IMMEDIATE`; `remove_device` apaga a row e não a recria. Ambos
      chamam `purge_account_state`, que varre `ACCOUNT_SCOPED_TABLES`
      (lista com teste de cobertura de schema upstream).
      **Obrigação do chamador, documentada e deliberada:** nem `reset_device`
      nem `remove_device` invalidam um handle já aberto para aquele id — uma
      sessão ainda escrevendo pelo handle antigo pode repopular o que acabou
      de ser limpo. Quem chama (`StoreRegistry`, e por trás dele o daemon)
      tem que parar completamente a sessão/writer daquela conta antes de
      chamar reset/remove. Isso é política upstream, não faltando tratar.
- [x] Reapontar o client para a revisão upstream que contém WR-1
      (`3a08e84e8d7a462f6f87eb98d28fe1ce889a653a`, via `cargo update` nas
      cinco crates git juntas — nunca pinadas individualmente). Nenhum SQL
      raw contra a tabela privada `device` no client; `StoreRegistry` é o
      único ponto de conversão `AccountId <-> device_id`.
- [x] **Topologia de pool: uma pool compartilhada, não uma por conta.**
      `StoreRegistry` abre um único `SqliteStore` base e deriva o handle de
      cada conta via `share_for_device(id)`, em vez de abrir N pools
      independentes com `new_for_device`. Isso não é otimização: duas pools
      independentes apontando pro mesmo arquivo físico podem entrar no
      deadlock de upgrade leitura-depois-escrita que o próprio
      `SqliteStoreConfig::pool_size` documenta upstream — `busy_timeout` não
      resolve esse caso porque não é `SQLITE_BUSY` comum, é duas transações
      `DEFERRED` cada uma tentando promover pra writer ao mesmo tempo. Uma
      pool só faz duas contas serem o mesmo writer, então o deadlock não tem
      segunda parte pra acontecer com. Esse é o default que a seção 7 do
      plano pede ("começar compartilhado"), não o fallback "pool por device".
      Descoberto ao investigar uma falha real e intermitente
      (`"database is locked"`) num teste que escrevia em duas contas
      concorrentemente sobre o mesmo arquivo real — não era flakiness do
      teste, era o bug de arquitetura que o plano já avisava para evitar.
- [x] IPC v30: handshake com `ClientScope`, capacidades independentes e
      mensagens de control plane (`ControlHello`, listagem e snapshot de
      accounts).
- [x] Servidor resolve `Account { account }` no `AccountRegistry`, recusa
      account id desconhecido e impede requests de control em conexões de conta
      e requests de conta em conexões de control.
- [x] `CreateAccount`/`ResetAccount`/`RemoveAccount` respondem de verdade em
      `serve_control_client`: a primeira passa pelo `AccountSupervisor`
      (`create_and_spawn`, resposta `AccountCreated`) e as outras duas
      despacham `Action::ForgetSession(disposition)` para o `Commands` da conta
      alvo (resposta `Accepted`; id desconhecido responde `NoSession`). A
      *storage* por trás (`StoreRegistry::create_account/reset_account/
      remove_account`) já existia e está testada.
- [x] Preparação única do schema do `chat-store`: `StoreRegistry` guarda um
      `OnceCell` compartilhado, `AccountRuntime`/`WhatsAppClient` recebem o
      mesmo `StoreRegistry` e cada runtime abre apenas seu próprio writer com
      `ChatStore::new_prepared`. N runtimes iniciando ao mesmo tempo não disparam
      N migration runners contra o mesmo arquivo.
- [x] **`ForgetSession` ("clear data and pair again") já usa `reset_account`,
      não mais `wipe_local_state()`.** A chamada antiga apagava o arquivo
      `whatsapp.db` inteiro, o que derrubaria qualquer outra conta local que
      compartilhasse o arquivo — incompatível com multi-account por
      definição. A ordem de teardown que já existia (fechar sessão, esperar
      `close()`, juntar plugins, parar publisher, retirar approvals de
      plugin, só então mexer em storage) já cumpre a obrigação do WR-1 acima;
      só o último passo mudou de "apagar o arquivo" para "chamar
      `StoreRegistry::reset_account(id)`" (que também dispara o `ON DELETE
      CASCADE` da migration de chat-store, então as linhas do chat-store
      dessa conta somem de graça na mesma transação).
- [x] **Plugin state namespaced por conta (nativo).** `approvals`/`settings`
      de plugin agora vivem em `plugin-state/{account_id}/`
      (`crates/daemon/src/plugins/mod.rs::account_state_dir`), não mais num
      diretório único compartilhado — sem isso, uma segunda conta real
      herdaria ou sobrescreveria as permissões da primeira. O catálogo de
      módulos `.wasm` continua global, como a seção 13.3 do plano pede. Web
      ainda não: `Origin`/`localStorage` continuam com um único prefixo não
      namespaced por conta — sinalizado como pendência no próprio plano.
- [x] **Shutdown agora acorda todas as contas, não só uma.**
      `crate::shutdown::ShutdownSignal` trocou de `tokio::sync::Notify`
      (`notify_one` acorda exatamente um waiter — invisível com uma única
      tarefa de sessão, quebrado no dia em que N contas cada uma espera o
      próprio `shutdown.notified()`) para um `watch<bool>` interno, onde
      todo waiter observa o mesmo request. API pública
      (`request()`/`requested()`) inalterada; pré-requisito que o item 4 do
      bloqueio abaixo já sinalizava, agora resolvido antes do primeiro spawn
      dinâmico de segunda conta em produção.
- [x] **`Action::ForgetSession` agora carrega uma `AccountDisposition`**
      (`Reset` ou `Remove`) em vez do bool antigo. `RuntimeLifecycle` ganhou
      um slot "first-call-wins"; o teardown em `session_bridge::run()` faz
      match nela para chamar `StoreRegistry::reset_account` (mantém o id,
      respawna) ou `StoreRegistry::remove_account` (aposenta o id de vez) —
      antes só a forma `Reset` existia. Os dois call sites atuais
      (self-service "clear data and pair again" e seu teste) continuam
      passando `Reset`, comportamento idêntico ao anterior.
- [x] **`AccountSupervisor` (nativo) implementado e ligado** em
      `crates/daemon/src/account/mod.rs`: `spawn(id)` monta hub/plugins/
      commands, registra e entrega a runtime ao reaper, que é o dono único do
      `JoinSet` — a tarefa de cada conta só roda a sessão e devolve um
      `AccountExit`, nunca chama `spawn` por dentro, o que elimina o deadlock
      entre `join_all()` e um respawn de `Reset` e o stall que um `spawn` de
      segunda conta sofria atrás de uma sessão longa. `spawn_with_hub(id, hub)`
      serve o bootstrap do `main.rs`, que no macOS constrói o `StateHub` na
      thread principal antes do runtime assíncrono (a tray). O reaper decide
      pelo `AccountExit`, não pelo pedido: `ResetCompleted` remove+respawna o
      mesmo id, `RemoveCompleted` só remove, `ResetIncomplete`/
      `RemoveIncomplete` deixam a runtime intacta, `SessionEnded` recupera com
      backoff e `SessionLoggedOut` deixa a conta para o usuário parear de novo.
      Gated `#[cfg(not(target_family = "wasm"))]`: `tokio::task::JoinSet`
      exige futures `Send`, e o host de plugin web é construído com closures
      `wasm-bindgen` deliberadamente `!Send` — `embedded.rs` continua com sua
      construção manual de uma conta só por esse motivo (item 8 do plano,
      supervisor `MaybeSend` próprio para web, ainda não feito).
- [x] **`DaemonMessage::AccountCreated { id, account }`** responde
      `ClientRequest::CreateAccount` no wire, nomeando o id que o daemon
      alocou — um cliente pode abrir uma conexão `Account`-scoped a ele
      assim que a mensagem chega, sem esperar `ListAccounts`/
      `AccountsChanged`. Comentários de doc das três requests de lifecycle
      não mencionam mais "recusado até WR-1" (WR-1 está mergeado).
- [x] **CI do PR #172 corrigido** (era vermelho antes deste commit):
      `account_device_cascade` e `message_stable_id` (que chegou via merge
      de `origin/main`, PR #171) compartilhavam o mesmo timestamp
      `2026-09-16-000000`. O tie-break do Diesel para duas migrations na
      mesma versão não é alfabético por nome de pasta — empiricamente,
      `account_device_cascade` rodava *depois* de `message_stable_id`,
      apesar de "account" < "message" — e o `pull_request` trigger do GitHub
      testa o merge commit, não só o head da branch, então o conflito só
      apareceu no CI. Corrigido renomeando `account_device_cascade` para
      `2026-09-15-235959` (um segundo antes da meia-noite do dia anterior),
      e carregando a FK `device_id -> device(id)` que `account_device_cascade`
      adiciona para dentro do up.sql/down.sql de `message_stable_id` também
      (que reconstrói `messages` por cima e por isso é quem decide a forma
      final da tabela). Dois helpers de teste do chat-store também
      precisaram da tabela/row pai `device` que essa FK passou a exigir em
      todo caminho de migração (`queries.rs::plan_bound`,
      `storage_shape.rs::file_store`).

### Validação registrada

- `cargo fmt --all -- --check` — passou.
- `cargo test --workspace --lib -- --test-threads=1` — passou por completo
  (12 crates, ~900 testes) após a mudança de topologia de pool e o rewiring
  de `reset_account`/plugin-state.
- `cargo test --workspace --all-features --lib -- --test-threads=1` — passou
  por completo.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` —
  sem warnings.
- `cargo check --workspace --all-targets` — passou.
- `cargo check -p oxidezap-daemon -p oxidezap-session --lib --target
  wasm32-unknown-unknown` — passou (mesmo alvo que o job `Test (web)` do CI
  compila antes de rodar no browser).
- `RUSTFLAGS="--cfg web_sys_unstable_apis" cargo clippy -p oxidezap-audio -p
  oxidezap-session --target wasm32-unknown-unknown -- -A clippy::all -A
  warnings -D clippy::disallowed_methods -D clippy::disallowed_types` —
  reproduz o "shared-view ban" do CI; passou.
- `cargo test -p oxidezap-session --lib -- --test-threads=1` — passou
  (169/169 antes das mudanças de plugin-state, que não tocam essa crate).
- Testes novos do `StoreRegistry`: `create_account_allocates_and_is_listed`,
  `ids_are_distinct_and_never_reused_after_removal`,
  `remove_retires_the_id_and_spares_other_accounts`,
  `reset_keeps_the_id_and_does_not_touch_other_accounts` — todos passam
  contra um arquivo temporário real (não o URL `cache=shared` em memória que
  o resto da suíte usa; ver nota de topologia de pool acima para o motivo).

### Validação desta sessão (fix de CI + `AccountSupervisor`)

- Após o merge de `origin/main` (traz o PR #171, `message_stable_id`) e a
  correção de ordenação de migrations: `cargo test -p oxidezap-chat-store
  --all-features` — passou por completo (44+ testes na suíte inline, todos os
  arquivos de teste de integração).
- `cargo test --workspace --all-features --no-fail-fast` — passou, com **uma
  única exceção pré-existente e não relacionada**:
  `whatsapp::tests::a_session_is_never_observed_half_open` (em
  `oxidezap-session`) é flaky sob execução paralela completa — confirmado
  isolado (`--exact`, passa sempre) e confirmado **também flaky em
  `origin/main` sem nenhuma mudança desta branch** (testado num worktree
  descartável). Não é uma regressão; não foi tocado.
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`, `cargo test --workspace --all-features
  --doc`, `cargo check --workspace --all-targets` — todos passaram limpos.
- `cargo test -p oxidezap-daemon -p oxidezap-session --lib --target
  wasm32-unknown-unknown --no-run` — falha no link ("--shared-memory is
  disallowed... not compiled with 'atomics'"), confirmado como limitação
  pré-existente deste sandbox local (reproduz igual num checkout limpo de
  `origin/main`); nenhum erro de compilação novo, só esse estágio de link.
  CI real (GitHub Actions) não roda esse comando exato da mesma forma —
  confiar no resultado do `Test (web)` do CI para essa parte.
- `RUSTFLAGS="--cfg web_sys_unstable_apis" cargo clippy -p oxidezap-audio -p
  oxidezap-session --target wasm32-unknown-unknown -- -A clippy::all -A
  warnings -D clippy::disallowed_methods -D clippy::disallowed_types` —
  passou.
- Diagnóstico completo do porquê o CI estava vermelho está no histórico de
  commit `fix(chat-store): make the cascade migration sort unambiguously
  before stable-id` na branch.

### Validação desta sessão (fiação do `AccountSupervisor`)

- Depois de ligar `AccountSupervisor` em `main.rs` (startup/shutdown) e nas
  três requests de lifecycle em `serve_control_client`: `cargo test -p
  oxidezap-daemon --all-features` — passou por completo (224+ testes na
  suíte inline, mais os arquivos de teste de integração), incluindo os dois
  testes novos (`supervisor_attaches_itself_to_its_registry_and_lets_it_go_when_dropped`
  em `account/mod.rs`; `create_account_is_refused_without_a_supervisor_attached`
  e `a_control_connection_resets_and_removes_a_named_account` em
  `server/tests.rs`) — rodados 3x seguidas para descartar flakiness, todas
  estáveis.
- `cargo check -p oxidezap-daemon --lib --target wasm32-unknown-unknown` —
  passou: `serve_control_client` e seus dois novos helpers
  (`create_account`/`change_account`) compilam para wasm, onde
  `AccountSupervisor` não existe — o split de `cfg` fica dentro de
  `create_account`, nenhum listener precisou de um branch próprio.
- Matriz completa novamente: `cargo fmt --all -- --check`, `cargo clippy
  --workspace --all-targets --all-features -- -D warnings`, `cargo test
  --workspace --all-features --no-fail-fast` (mesma única exceção
  pré-existente de antes), `cargo check --workspace --all-targets`, `cargo
  test --workspace --all-features --doc`, os dois comandos de wasm do CI —
  todos passaram.

## Bloqueio arquitetural atual

~~A implementação de lifecycle de contas depende do WR-1 no
`whatsapp-rust`.~~ **Resolvido**. ~~Falta orquestração em tempo de execução
no daemon (`AccountSupervisor`).~~ **Resolvido**. ~~Falta ligar
`AccountSupervisor` em `main.rs` e nas três mutations do control plane.~~
**Resolvido nesta rodada**: `main.rs` agora usa `AccountSupervisor` no
startup (lista `StoreRegistry::accounts()`, spawna a conta legada pelo hub
pré-construído e as demais por `spawn`) e no shutdown (`join_all()` no lugar
do `JoinHandle` único; `shutdown::request()` no lugar do `Arc<Notify>`
local). `serve_control_client` responde de verdade `CreateAccount`
(`AccountSupervisor::create_and_spawn`, resposta `AccountCreated`) e
`ResetAccount`/`RemoveAccount` (despacha `Action::ForgetSession(disposition)`
para o `Commands` da conta alvo, resposta `Accepted`; id desconhecido
responde `NoSession`). `AccountRegistry` carrega um `Weak<AccountSupervisor>`
(setado por `AccountSupervisor::new`/`with_shutdown`) para que a conexão de
controle alcance o supervisor sem que nenhum listener precise passar um
segundo `Arc` — `Weak` porque o supervisor já segura um
`Arc<AccountRegistry>`, e um ponteiro forte de volta seria um ciclo que
nenhum dos dois solta.

**Não sobrou nenhuma peça de arquitetura pendente para o lifecycle dinâmico
de contas no daemon nativo.** O que falta agora é a camada acima:

1. **A GUI depende de tudo isso**: `ControlSession`/`AccountWorkspace`,
   switcher, Add/Reset/Remove na UI — hoje a GUI nem fala v30 do jeito
   multi-conta, só usa o wrapper de conta legada.
2. **Web/embedded continuam de fora**: `embedded.rs` continua construindo um
   `AccountRuntime` único à mão (não usa `AccountSupervisor`, que exige
   `Send` e o host de plugin web não é); o registry ali nunca tem um
   supervisor anexado, então as três mutations respondem `Refused` lá —
   correto para hoje, mas item 8 do plano (registry multi-conta próprio pra
   web) segue pendente.
3. **Coordenação global (tray, calls) ainda não existe**: o `hub`
   pré-construído passado a `spawn_with_hub` continua fixo em
   `AccountId::LEGACY`; a tray só observa esse hub. Com múltiplas contas
   reais, a tray precisa saber agregar mais de uma.

**Decisão de produto do item 9/seção 20 (perguntada e respondida): o que
acontece com uma chamada ativa quando o usuário troca de conta.** Opção
escolhida: **A — finalizar a chamada.** Trocar de conta encerra qualquer
chamada ativa na conta que está sendo deixada; a UI deve avisar
explicitamente antes de agir ("trocar de conta vai encerrar sua chamada
 atual") e só prosseguir com confirmação do usuário — nunca finalizar a
chamada silenciosamente. Nenhuma implementação de bloqueio restante: esta
é a resposta definitiva para quando o switcher (item 6/7) e a coordenação
global de chamada (item 9) forem implementados. Registrado aqui em vez de
no código porque não existe hoje nenhuma ação de "trocar de conta" na GUI
para este aviso ser anexado a — a GUI ainda fala só com a conta legada (ver
item 1 abaixo). Quando o switcher existir, a confirmação entra no mesmo
ponto onde `reset_and_pair_again`/`clear data and pair again`
(`crates/gui/src/app/mod.rs`) já encerra a sessão hoje: checar
`WhatsAppApp::active_call` antes de agir e, se houver uma, pedir
confirmação antes de chamar `hang_up` e prosseguir.

## Sequência de implementação

- [x] **1. Fundação** — `StoreRegistry` compartilhado (uma pool, não uma por
      conta), preparação única do DB, lifecycle completo (`create/reset/
      remove_account`) sobre WR-1, e testes de isolamento A/B no mesmo
      arquivo, incluindo reset/remove.
- [~] **2. Estado externo por conta** — namespace de media, avatar, wipe e
      **plugin-state (nativo)** por conta estão prontos; falta plugin state
      no web (`Origin`/`localStorage` ainda não é keyed por `AccountId,
      seção 13.3 do plano) e o staging global de instalação de plugin
      (seção 13.2).
- [x] **3. `AccountRuntime`** — encapsula `StateHub`, commands, plugin host e
      lifecycle por conta; o daemon/embedded/listeners compartilham um
      `AccountRegistry`, o bridge abre e reseta a sessão com o `AccountId`
      correto, e `AccountSupervisor` liga tudo isso em `main.rs`
      (spawn/respawn dinâmico, nativo). Falta só web/embedded (item 8) e a
      coordenação global do item 9 no bloqueio acima.
- [x] **4. `AccountRegistry`** — N runtimes e snapshot/status isolados,
      `AccountSupervisor` como supervisor de tarefa por conta, e o
      `Weak<AccountSupervisor>` que deixa `serve_control_client` alcançá-lo
      sem uma segunda `Arc` em cada listener — tudo pronto e testado.
- [x] **5. IPC v30** — scopes `Control`/`Account`, handshake com `AccountId`,
      conexão de conta imutavelmente bound, listagem do registry, enforcement
      de requests, e as três mutações de lifecycle (`CreateAccount` ->
      `AccountCreated`, `ResetAccount`/`RemoveAccount` -> `Accepted`) todas
      respondendo de verdade em `serve_control_client`.
- [ ] **6. GUI** — `ControlSession` + `AccountWorkspace`, attach/detach e
      completions assíncronas protegidas contra switch.
- [ ] **7. UX** — switcher, Add, Reset, Remove, pairing por runtime e estado
      mínimo da conta ativa.
- [ ] **8. Web/embedded** — registry singleton, Web Lock no daemon, um DB OPFS,
      scopes de tab e restauração do conjunto inteiro, plugin-state por conta.
- [ ] **9. Global** — tray agregado, `CallCoordinator`, sinais cross-account e
      política determinística para hardware de chamadas. Decisão de produto
      **já tomada** (não mais pendente): opção A, finalizar a chamada ativa
      ao trocar de conta, com aviso explícito antes de agir — ver a nota no
      "Bloqueio arquitetural atual" acima. Falta só a implementação, que
      depende do switcher (item 6/7) existir.
- [ ] **10. Hardening/performance** — matriz de races, testes de isolamento,
       stress de writer, benchmark 1/2/4 contas, web, docs e CI completo.

## Definition of done

- [ ] Duas ou mais contas conectadas simultaneamente no mesmo daemon e no
      mesmo `whatsapp.db`, com `device_id` distintos.
- [ ] Switch de conta troca somente a conexão frontend; nenhuma sessão WhatsApp
      é reconectada ou encerrada.
- [ ] Eventos, unread, mídia, plugins e permissões permanecem account-scoped.
- [x] Reset/remove de A não altera state, rows ou runtime de B/C no storage
      (testado); falta provar o mesmo em runtime (spawn dinâmico, item acima).
      Ids removidos nunca são reutilizados (testado, via `AUTOINCREMENT`).
- [ ] Operações assíncronas iniciadas em A não publicam em B após switch.
- [ ] Falha de uma runtime não derruba daemon nem as demais.
- [ ] Web, chamadas, tray, storage usage, migrations e benchmarks passam os
      gates do plano.
- [ ] `AGENTS.md`, `docs/architecture.md`, `docs/gotchas.md` e `docs/web.md`
      descrevem o invariante final.
