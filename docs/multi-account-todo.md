# Multi-account V2 — TODO

Fonte de requisitos: `multi-account-plan-v2.md` (handoff em `/home/jlucaso/Downloads`,
mantido em paralelo a este arquivo — atualizar os dois a cada marco).
A implementação usa **um `whatsapp.db` compartilhado**; `AccountId` é o `device.id`
positivo desse banco. Não reintroduzir `accounts.json`, UUIDs, um DB por conta ou
migração de arquivo.

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

## Bloqueio arquitetural atual

~~A implementação de lifecycle de contas depende do WR-1 no
`whatsapp-rust`.~~ **Resolvido**: WR-1 está mergeado e a dependência já
aponta pro commit que o contém. O que falta agora não é mais storage — é
orquestração em tempo de execução no daemon:

1. **Spawn de runtime dinâmico.** Hoje `main.rs`/`embedded.rs` cada um
   constrói manualmente exatamente um `AccountRuntime` inline e dá spawn em
   uma única tarefa `registry.run(...)`. Falta uma função reutilizável
   (`crates/daemon/src/account/mod.rs` é o lugar natural) que, dado um
   `AccountId` e o `StoreRegistry`/`AccountRegistry` compartilhados, monta
   `StateHub::for_account(id)`, plugins, canal de comandos, `AccountRuntime`
   e dá spawn no `run()` — usável tanto no startup (iterando
   `StoreRegistry::accounts()`) quanto sob demanda (`CreateAccount`).
2. **Startup deveria listar contas reais.** `main.rs`/`embedded.rs` hoje
   sempre criam um único runtime para `AccountId::LEGACY`. Uma vez que (1)
   exista, o startup deveria chamar `StoreRegistry::accounts()` e dar spawn
   em um runtime por linha existente, só criando `AccountId::LEGACY` se a
   lista vier vazia (primeiro launch).
3. **`CreateAccount` é o caso fácil**: aloca um id novo
   (`StoreRegistry::create_account()`, já testado), monta e registra um
   runtime nunca antes vivo — nenhum teardown envolvido, só (1).
4. **`ResetAccount`/`RemoveAccount` via control plane são o caso difícil**,
   porque envolvem parar um runtime *que já está rodando* a partir de uma
   conexão que não é a dele (a conexão de controle). Caminho de design já
   avaliado nesta sessão: a própria conta tem um `Commands` (canal de
   comandos) — reaproveitar o mesmo `Action::ForgetSession` que hoje só o
   cliente daquela conta manda para si mesmo, mas disparado pelo control
   plane contra o `Commands` de qualquer runtime, e então **esperar aquele
   runtime terminar** (hoje `main.rs` segura o `JoinHandle`, mas um pedido
   de reset vindo do control plane não tem acesso a essa stack frame — falta
   um supervisor no próprio `AccountRegistry`/novo tipo que guarde
   `JoinHandle` por conta). Depois de terminar: `ResetAccount` deve dar
   respawn (mesmo id, novo runtime, imediatamente reconectável) e
   `RemoveAccount` deve só remover do registry (linha já apagada pelo
   `remove_device`, id nunca reemitido).
   Cuidado de concorrência já identificado: o sinal de shutdown global do
   processo hoje usa `tokio::sync::Notify::notify_one()` (em `main.rs`), que
   só acorda **um** waiter — correto quando só existe uma tarefa de sessão,
   quebrado no dia em que N contas cada uma espera o próprio
   `shutdown.notified()`. Precisa virar `notify_waiters()` (ou um
   `CancellationToken`, ainda não é dependência do workspace) antes de
   qualquer spawn dinâmico de segunda conta em produção.
5. **A GUI depende de (1)-(4)**: `ControlSession`/`AccountWorkspace`,
   switcher, Add/Reset/Remove na UI.

Nenhum desses quatro pontos precisa mais esperar por upstream — são só
trabalho de orquestração local, e o storage por trás já está pronto e
testado.

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
      correto. Falta o spawn dinâmico (criar/parar/respawnar runtime em
      tempo de execução) — ver "Bloqueio arquitetural atual" acima.
- [~] **4. `AccountRegistry`** — N runtimes e snapshot/status isolados estão
      prontos para o daemon; falta o supervisor de `JoinHandle` por conta que
      o spawn dinâmico de reset/remove/create precisa.
- [~] **5. IPC v30** — scopes `Control`/`Account`, handshake com `AccountId`,
      conexão de conta imutavelmente bound, listagem do registry e enforcement
      de requests estão prontos; mutações de lifecycle ainda recusam,
      esperando o spawn dinâmico do item 3/4, não mais WR-1.
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
