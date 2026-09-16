# Multi-account V2 — TODO

Fonte de requisitos: `multi-account-plan-v2.md` (handoff em `/home/jlucaso/Downloads`,
mantido em paralelo a este arquivo — atualizar os dois a cada marco).
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
- [~] `CreateAccount`/`ResetAccount`/`RemoveAccount` estão presentes no wire
      (`ClientRequest`) e a *storage* por trás deles já existe e está testada
      (`StoreRegistry::create_account/reset_account/remove_account`), mas
      `serve_control_client` ainda responde `Refused` para as três — falta o
      "spawn/stop de runtime em tempo de execução" no daemon. Ver bloqueio
      arquitetural atual, abaixo.
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
- [x] **`AccountSupervisor` (nativo) implementado** em
      `crates/daemon/src/account/mod.rs`: `spawn(id)` monta hub/plugins/
      commands, registra e dá spawn no `run()` de uma conta — recursivo (um
      respawn de `Reset` chama `spawn` de novo de dentro da tarefa que
      `hold()` guarda), por isso retorna um `Pin<Box<dyn Future<...> +
      Send>>` explícito em vez de um `async fn` comum, que não compila
      recursivo (tamanho infinito). `spawn_with_hub(id, hub)` é a mesma
      coisa para o bootstrap do `main.rs`, que no macOS precisa construir o
      `StateHub` na thread principal antes do runtime assíncrono (a tray).
      `hold()` drena o `run()` de uma conta e decide pelo `disposition()`:
      `Reset` remove+respawna o mesmo id, `Remove` só remove,
      nenhuma disposição (shutdown ou sessão que terminou sozinha) deixa a
      runtime como o `run()` a deixou. `join_all()` drena todas as tarefas já
      dadas spawn, incluindo as que um `Reset` adicionou depois do startup.
      Gated `#[cfg(not(target_family = "wasm"))]`: `tokio::task::JoinSet`
      exige futures `Send`, e o host de plugin web é construído com closures
      `wasm-bindgen` deliberadamente `!Send` — `embedded.rs` continua com sua
      construção manual de uma conta só por esse motivo (item 8 do plano,
      supervisor `MaybeSend` próprio para web, ainda não feito).
      **Ainda não conectado a `main.rs`/`serve_control_client`** — ver
      "Bloqueio arquitetural atual" abaixo, que passa a ser só isso agora.
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

## Bloqueio arquitetural atual

~~A implementação de lifecycle de contas depende do WR-1 no
`whatsapp-rust`.~~ **Resolvido**: WR-1 está mergeado e a dependência já
aponta pro commit que o contém. ~~Falta orquestração em tempo de execução
no daemon (`AccountSupervisor`).~~ **Resolvido**: `AccountSupervisor` existe
e está testado em isolamento (ver item acima) — o `ShutdownSignal` que
precisava virar broadcast antes de qualquer spawn dinâmico em produção
também já foi corrigido. O que falta agora é só **fiação** (nenhuma peça
nova de arquitetura, só ligar o que já existe):

1. **Conectar `AccountSupervisor` ao `main.rs`.** Hoje `main.rs` ainda
   constrói manualmente exatamente um `AccountRuntime` inline para
   `AccountId::LEGACY` e segura seu `JoinHandle` num `select!` — o processo
   inteiro vive ou morre com essa única sessão. Precisa virar: no startup,
   listar `StoreRegistry::accounts()`; se vazia, `AccountId::LEGACY` (primeiro
   launch); para a primeira conta (ou a única), usar
   `AccountSupervisor::spawn_with_hub` (o hub pré-construído na main thread
   pro tray do macOS); para as demais, `AccountSupervisor::spawn`. No
   shutdown, `supervisor.join_all()` no lugar do join único; o `stop:
   Arc<Notify>` local do `main.rs` deveria sumir em favor de
   `crate::shutdown` (agora broadcast-safe).
2. **Conectar `CreateAccount`/`ResetAccount`/`RemoveAccount` no
   `serve_control_client`** (`crates/daemon/src/server/mod.rs`), que hoje
   ainda responde `Refused` para as três. Design já fechado, reaproveitando
   peças que já existem e já são testadas:
   - `CreateAccount`: `stores.create_account()` → id, depois
     `supervisor.spawn(id).await`, responde
     `DaemonMessage::AccountCreated { id, account }`.
   - `ResetAccount { account }` / `RemoveAccount { account }`: acha a
     `AccountRuntime` alvo via `registry.get(account)`, monta um
     `SessionCommand { action: Action::ForgetSession(Reset|Remove), reply }`
     e manda pelo `Commands` **daquela** conta — reaproveitando exatamente o
     mesmo caminho de teardown self-service que o cliente da própria conta já
     usa, nenhum código novo de teardown — espera o oneshot de resposta,
     responde `DaemonMessage::Accepted { id }`. O stop+respawn/remove
     de fato acontece depois, em background, via `AccountSupervisor::hold()`
     (já implementado). Falta decidir onde o `Arc<AccountSupervisor>` fica
     acessível a partir de onde a conexão de controle roda hoje (ao lado do
     `Arc<AccountRegistry>`, ou dentro dele).
3. **A GUI depende de (1)-(2)**: `ControlSession`/`AccountWorkspace`,
   switcher, Add/Reset/Remove na UI.

Nenhum desses pontos precisa mais de nenhuma peça de arquitetura nova —
`AccountSupervisor`, `AccountDisposition` e o protocolo de wire
(`AccountCreated`) já existem e já passam na sua própria suíte; falta ligar
tudo isso ao processo real (`main.rs`) e ao servidor de controle.

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
- [~] **3. `AccountRuntime`** — encapsula `StateHub`, commands, plugin host e
      lifecycle por conta; o daemon/embedded/listeners já compartilham um
      `AccountRegistry` e o bridge abre e reseta a sessão com o `AccountId`
      correto. `AccountSupervisor` (spawn/respawn dinâmico) já existe e está
      testado — falta só ligá-lo em `main.rs` ("Bloqueio arquitetural atual"
      acima, item 1).
- [x] **4. `AccountRegistry`** — N runtimes e snapshot/status isolados estão
      prontos para o daemon; `AccountSupervisor` é o supervisor de tarefa por
      conta que o spawn dinâmico de reset/remove/create precisa — falta só a
      fiação em `main.rs`/`serve_control_client`.
- [~] **5. IPC v30** — scopes `Control`/`Account`, handshake com `AccountId`,
      conexão de conta imutavelmente bound, listagem do registry, enforcement
      de requests e a resposta `AccountCreated` estão prontos; as três
      mutações de lifecycle ainda respondem `Refused` em
      `serve_control_client` — falta só a fiação (item 2 do bloqueio acima),
      não mais nenhuma peça de arquitetura.
- [ ] **6. GUI** — `ControlSession` + `AccountWorkspace`, attach/detach e
      completions assíncronas protegidas contra switch.
- [ ] **7. UX** — switcher, Add, Reset, Remove, pairing por runtime e estado
      mínimo da conta ativa.
- [ ] **8. Web/embedded** — registry singleton, Web Lock no daemon, um DB OPFS,
      scopes de tab e restauração do conjunto inteiro, plugin-state por conta.
- [ ] **9. Global** — tray agregado, `CallCoordinator`, sinais cross-account e
      política determinística para hardware de chamadas. Decisão de produto
      pendente e explicitamente para perguntar ao usuário antes de
      implementar: o que acontece com uma chamada ativa quando o usuário
      troca de conta (seção 20 do plano).
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
