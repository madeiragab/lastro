[Português](09-roadmap.md) · [English](../en/09-roadmap.md) · [↑ README](../../README.md)

# 09 · Roadmap

Seis etapas. Cada uma só começa quando a anterior passa no próprio critério de pronto.

```mermaid
flowchart LR
    M0["0 · Esqueleto"] --> M1["1 · Pager"]
    M1 --> M2["2 · B+Tree"]
    M2 --> M3["3 · WAL"]
    M3 --> M4["4 · SQL"]
    M4 --> M5["5 · MVCC"]
    M5 --> M6["6 · Provas"]
```

Os prazos são estimativa de quem está aprendendo Rust e banco de dados ao mesmo tempo, em ritmo
de projeto paralelo. Servem para detectar desvio, não como promessa.

---

## Etapa 0 · Esqueleto — 1 semana

`cargo init`, layout de módulos, tipos base (`PageId`, `Lsn`, `TxId`), tipo de erro,
integração contínua rodando, `README` publicado.

**Pronto quando:** `cargo test` passa com um teste trivial e a integração contínua está verde.

---

## Etapa 1 · Pager e buffer pool — 4 a 6 semanas

Inclui aprender Rust. A curva é real, e essa etapa é o lugar certo para pagá-la, porque o domínio
é o mais simples do projeto.

Entrega: `Pager` com leitura, escrita, alocação e freelist. `BufferPool` com política clock,
`PageGuard` com `Drop`. Layout de slotted page, varint, codificação de chave que preserva ordem.

**Pronto quando** ([detalhes](03-pager.md#critério-de-pronto)):
- seis invariantes verificadas em `proptest` com 10 mil casos
- um milhão de operações contra o modelo, sem divergência
- injeção de falha de E/S em cada ponto de escrita
- nenhum vazamento de pin em qualquer caminho de erro

---

## Etapa 2 · B+Tree — 6 a 8 semanas

A etapa mais longa. Split é tranquilo, merge não é.

Entrega: busca, inserção com split, remoção com merge e rebalanceamento, range scan por ponteiro
de irmão, cadeia de overflow, `check_tree()` verificando as sete invariantes.

**Pronto quando** ([detalhes](04-btree.md#critério-de-pronto)):
- um milhão de operações contra `BTreeMap`, sem divergência
- `check_tree()` após cada operação em modo debug
- os cinco padrões adversariais no conjunto fixo
- range scan concordando com a travessia hierárquica

**Ponto de corte:** se merge e rebalanceamento passarem de duas semanas travados, entrega
tombstone mais compactação offline, registra a limitação no README e segue. A limitação declarada
custa menos que o cronograma inteiro.

---

## Etapa 3 · WAL e recovery — 6 a 8 semanas

O coração. Não corta, não simplifica, não adia.

Entrega: formato do registro, `WalWriter` com a regra WAL aplicada no buffer pool, checkpoint
fuzzy, as três fases do ARIES, CLRs, e o crash fuzzer com o verificador de quatro perguntas.

**Pronto quando** ([detalhes](05-wal-recovery.md#critério-de-pronto)):
- 50 mil sementes sem violação de atomicidade
- queda injetada em cada uma das três fases, com o recovery seguinte concluindo
- recovery dez vezes seguidas sobre o mesmo log, estado idêntico
- log truncado em cada byte de um registro, sempre abrindo sem pânico

Quando esta etapa fecha, existe um banco de dados de verdade — sem SQL, mas transacional e
durável, o que é a parte difícil.

---

## Etapa 4 · SQL — 5 a 7 semanas

A camada mais volumosa em linhas de código e a mais simples conceitualmente. Nada aqui pode
corromper dados.

Entrega: lexer, parser recursivo descendente, binder, catálogo em tabelas do próprio banco,
planner com as seis regras, executor com onze operadores, `EXPLAIN`, `lastro-cli`.

**Pronto quando** ([detalhes](06-sql.md#critério-de-pronto)):
- a gramática inteira aceita, entrada malformada rejeitada com posição
- fuzzer de parser com cem mil strings sem pânico
- round-trip AST para SQL e de volta
- `EXPLAIN` cobrindo as seis regras
- lógica de três valores conferida contra a tabela verdade

---

## Etapa 5 · MVCC — 4 a 5 semanas

Entrega: `xmin` e `xmax` no cabeçalho da tupla, snapshot, regra de visibilidade, detecção de
conflito *first-updater-wins*, vacuum com horizonte.

**Pronto quando** ([detalhes](07-mvcc.md#critério-de-pronto)):
- tabela verdade completa da visibilidade em teste fixo
- bateria de anomalias produzindo exatamente a tabela declarada, write skew incluso
- vacuum coletando toda versão morta e nenhuma viva
- crash fuzzer com carga MVCC concorrente

---

## Etapa 6 · Provas e publicação — 3 a 4 semanas

Entrega: runner do sqllogictest com relatório de denominador honesto, bateria de anomalias
completa, benchmark contra SQLite com perfil de execução, README preenchido com os números
medidos, série de devlogs.

**Pronto quando:**
- relatório do sqllogictest publicado, com filtragem declarada
- tabela de anomalias completa no README
- gráficos de benchmark com `flamegraph` e análise da derrota
- um devlog por camada, publicado

---

## Resumo

| Etapa | Estimativa | Acumulado |
|---|---|---|
| 0 · Esqueleto | 1 semana | 1 |
| 1 · Pager | 4 a 6 semanas | 5 a 7 |
| 2 · B+Tree | 6 a 8 semanas | 11 a 15 |
| 3 · WAL | 6 a 8 semanas | 17 a 23 |
| 4 · SQL | 5 a 7 semanas | 22 a 30 |
| 5 · MVCC | 4 a 5 semanas | 26 a 35 |
| 6 · Provas | 3 a 4 semanas | 29 a 39 |

**Sete a nove meses** em ritmo de projeto paralelo. O horizonte inicial de cinco meses era
otimista; esta tabela é a estimativa honesta depois de escrever a especificação.

Cortando merge da B+Tree e reduzindo o escopo de SQL, cabe em cinco. Cortando o WAL, caberia em
três — e não seria mais um banco de dados.

---

## Depois

Fora do escopo desta versão, registrado para não virar escopo por acidente:

- `GROUP BY` e funções de agregação
- Subconsultas e CTE
- Planner baseado em custo, com estatísticas e histogramas
- Serializable snapshot isolation, fechando o write skew
- Compressão de página
- Replicação por envio de WAL
- Backup incremental a partir do log

---

Anterior: [08 · Testes e provas](08-testes.md) · Próximo: [10 · Glossário](10-glossario.md)
