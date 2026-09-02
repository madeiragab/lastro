# lastro

**Português** · [English](README.en.md)

[![CI](https://github.com/madeiragab/lastro/actions/workflows/ci.yml/badge.svg)](https://github.com/madeiragab/lastro/actions/workflows/ci.yml)

**Um banco de dados relacional embutido, escrito do zero em Rust.**

Páginas em disco, B+Tree, write-ahead log com crash recovery, parser SQL e MVCC.
Sem dependência de engine externa. O objetivo não é competir com o SQLite — é entender,
linha por linha, o que um banco de dados faz entre o seu `INSERT` e o dado estar seguro no disco.

---

## Status

Em construção. Nada aqui é estável, e as tabelas de resultado estão vazias de propósito —
número só entra depois de medido.

| Camada | Estado |
|---|---|
| Especificação e documentação | concluída |
| Formato de arquivo, slotted page, codificações | concluído |
| Pager e freelist | concluído |
| Buffer pool com política clock | concluído |
| B+Tree | concluído |
| WAL + crash recovery | não começado |
| SQL (parser, planner, executor) | não começado |
| MVCC / snapshot isolation | não começado |
| Suíte de provas | parcial: modelo e propriedade prontos, crash fuzzer não |

O que já roda: criar e abrir um arquivo `.lastro`, alocar e liberar páginas com reuso pela
freelist, guardar células de tamanho variável em slotted pages com compactação, ler e escrever
tudo isso através de um buffer pool de tamanho fixo, e inspecionar o arquivo pelo `lastro-cli`.

Sem dependência nenhuma na biblioteca: CRC32C, varint e as codificações são escritos aqui.

---

## Documentação

O projeto foi especificado antes de ser escrito. O formato binário do arquivo, o formato do
registro de log e as invariantes de cada estrutura estão definidos abaixo.

| Documento | Assunto |
|---|---|
| [01 · Arquitetura](docs/pt/01-arquitetura.md) | As camadas, o que cada uma esconde da de cima |
| [02 · Formato de arquivo](docs/pt/02-formato-de-arquivo.md) | Layout binário: header, slotted page, células, codificação de chave |
| [03 · Pager e buffer pool](docs/pt/03-pager.md) | Páginas, pin/unpin, política clock, freelist |
| [04 · B+Tree](docs/pt/04-btree.md) | Busca, split, merge, range scan, invariantes |
| [05 · WAL e recovery](docs/pt/05-wal-recovery.md) | Formato do log, regra WAL, as três fases do ARIES |
| [06 · SQL](docs/pt/06-sql.md) | Gramática, planner, operadores do executor |
| [07 · MVCC](docs/pt/07-mvcc.md) | Versionamento, snapshot, regra de visibilidade, coleta |
| [08 · Testes e provas](docs/pt/08-testes.md) | Crash fuzzer, sqllogictest, anomalias, benchmark |
| [09 · Roadmap](docs/pt/09-roadmap.md) | Ordem de construção e critério de pronto por camada |
| [10 · Glossário](docs/pt/10-glossario.md) | Vocabulário de banco de dados, sem enrolação |
| [ADR](docs/pt/adr.md) | Decisões de arquitetura e o que foi descartado |

---

## O que é e o que não é

**É:** um banco embutido single-node e single-writer, no espírito do SQLite. Um arquivo, uma
biblioteca, sem servidor. Transacional e durável de verdade — não um dicionário salvo em disco.

**Não é:** distribuído, replicado, nem otimizado para vencer benchmark. Não tem planner baseado
em custo, nem otimizador de junção, nem paralelismo intra-query. Cada uma dessas coisas é um
projeto inteiro, e um projeto raso em cinco frentes vale menos que um projeto sério em uma.

---

## Arquitetura em uma imagem

```mermaid
flowchart TD
    SQL["SQL de entrada"] --> LEX["Lexer e parser"]
    LEX --> AST["AST"]
    AST --> PLAN["Planner"]
    PLAN --> EXEC["Executor - modelo iterator"]
    EXEC --> TXN["Gerenciador de transacoes - MVCC"]
    TXN --> ACCESS["Metodos de acesso - B+Tree e heap"]
    ACCESS --> BUF["Buffer pool"]
    BUF --> PAGER["Pager - paginas de 4 KB"]
    PAGER --> DISK[("arquivo .lastro")]
    TXN --> WAL["Write-ahead log"]
    WAL --> WALFILE[("arquivo .wal")]
    WALFILE -.->|recovery no boot| TXN
```

Detalhes em [01 · Arquitetura](docs/pt/01-arquitetura.md).

---

## O coração do projeto

A pergunta que move tudo: **o que acontece se a máquina morrer exatamente no meio de um `COMMIT`?**

A resposta certa é que ou a transação inteira aconteceu, ou nenhuma parte dela aconteceu. Nunca
metade. A única forma honesta de afirmar isso é testando — e é para isso que existe o
**crash fuzzer**:

> O processo mata a si mesmo, sem chance de limpar nada, em um ponto aleatório dentro do caminho
> de commit. Reabre o banco. Roda recovery. Um verificador confere que o estado é exatamente ou o
> anterior à transação, ou o posterior a ela. Repete dezenas de milhares de vezes na integração
> contínua.

Escrever um banco é a parte fácil. Provar que ele não perde seus dados é o projeto de verdade.
Detalhes em [05 · WAL e recovery](docs/pt/05-wal-recovery.md) e [08 · Testes](docs/pt/08-testes.md).

---

## Como rodar

Testes, incluindo os de modelo e os baseados em propriedade:

```bash
cargo test
```

Cria um banco vazio:

```bash
cargo run --bin lastro-cli -- create exemplo.lastro
```

Lê a página de metadados e confere as invariantes:

```bash
cargo run --bin lastro-cli -- info exemplo.lastro
```

Resume todas as páginas do arquivo, ou abre uma:

```bash
cargo run --bin lastro-cli -- pages exemplo.lastro
```

```bash
cargo run --bin lastro-cli -- page exemplo.lastro 1
```

---

## Provas

Nenhum número entra aqui sem ter sido medido, com o comando ao lado. Metodologia completa em
[08 · Testes e provas](docs/pt/08-testes.md).

**Compatibilidade** — a suíte SQL Logic Test do SQLite, escrita por terceiros, rodada contra o
subconjunto de SQL implementado aqui.

| Métrica | Valor |
|---|---|
| Testes executados | pendente |
| Aprovados | pendente |

**Correção transacional** — as anomalias clássicas de isolamento. O objetivo não é passar em
todas: snapshot isolation permite *write skew* por definição. A tabela mostra o que é prevenido
e o que não é, porque um banco que mente sobre o próprio isolamento é pior que um banco lento.

| Anomalia | Prevenida? |
|---|---|
| Dirty read | pendente |
| Non-repeatable read | pendente |
| Phantom read | pendente |
| Lost update | pendente |
| Write skew | pendente |

**Desempenho** — comparação com SQLite nas mesmas cargas. Expectativa: o `lastro` perde por
margem larga. O SQLite tem 25 anos de otimização. Os gráficos vão ser publicados perdendo, junto
da análise de onde o tempo vai embora. Benchmark interessante não é o que mostra quem ganhou, é
o que explica por quê.

---

## Diário de bugs

Registro dos erros que custaram caro, porque é a parte que de fato ensinou alguma coisa.

### A invariante que estava errada duas vezes

A especificação afirmava que todo nó da B+Tree fora da raiz estaria pelo menos 40% cheio. É o
que todo livro diz, e é falso aqui.

**Primeira tentativa.** Escrevi o limiar de 40%, e ele não sobrevive a células de tamanho
variável: uma única célula pode ocupar um terço da página, então uma divisão perfeitamente
equilibrada deixa as duas metades abaixo do piso sem nada estar errado.

**Segunda tentativa.** Troquei por algo mais forte e aparentemente à prova de bala: *nenhum par
de irmãos adjacentes cabe junto em uma única página* — ou seja, nada que poderia ter sido fundido
ficou sem fundir. Escrevi a verificação, rodei o teste de propriedade, e ela falhou.

O caso mínimo que o proptest reduziu **não tinha nenhuma remoção.** Só inserções.

O motivo, óbvio depois: quando um nó cheio racha ao meio, cada metade fica com meia página. Uma
delas agora está ao lado de um vizinho intocado — um vizinho ao qual ela nunca precisou caber
junto, porque antes do split ela fazia parte de um nó grande. Split cria pares fundíveis. Não é
bug de remoção, é a natureza da inserção.

**O que ficou.** O fator de preenchimento não é afirmado, é **medido**, por `BTree::stats`, e os
testes afirmam sobre a medida. A invariante 4 virou algo modesto e verdadeiro: nenhum nó fora da
raiz está vazio.

A lição não é sobre B+Tree. É que uma invariante escrita a partir do que o livro diz, sem ser
executada contra entrada aleatória, é uma hipótese — e as duas primeiras hipóteses aqui estavam
erradas por motivos diferentes.

### O irmão da direita que nunca era olhado

Achado pelo mesmo teste, antes do anterior. O rebalanceamento após uma remoção só tentava fundir
o nó com o irmão da **esquerda**. Um nó que poderia ter fundido para a direita ficava parado.

Nada quebra de forma visível quando isso acontece. Toda consulta continua devolvendo a resposta
certa; a árvore só vai ficando mais esparsa do que deveria, para sempre. É o tipo de defeito que
teste de comportamento nunca pega, e a razão de a verificação de invariante existir.

---

## Referências

- **CMU 15-445 / 15-721**, Andy Pavlo — o currículo desse projeto, aula por aula, em vídeo aberto
- **Database Internals**, Alex Petrov — B-tree e log de recuperação em profundidade
- **Architecture of SQLite** — `sqlite.org/arch.html`
- **ARIES**, Mohan et al., 1992 — o artigo original de write-ahead logging
- **Rust Atomics and Locks**, Mara Bos — para a camada de concorrência

---

## Licença

MIT.
