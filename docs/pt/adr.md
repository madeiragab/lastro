[Português](adr.md) · [English](../en/adr.md) · [↑ README](../../README.md)

# Registro de decisões de arquitetura

Cada decisão traz o contexto, as alternativas consideradas e o que se perde com a escolha. Uma
decisão sem alternativa registrada não é decisão, é acidente.

---

## ADR-001 · Rust

**Estado:** aceita

**Contexto.** Um banco de dados manipula bytes crus, gerencia memória explicitamente e precisa de
latência previsível. As linguagens que eu já sei — Python e JavaScript — não servem para o caminho
quente.

**Alternativas.**

*C* — a linguagem clássica do domínio. SQLite é C. Gerenciamento manual sem rede de proteção, e
todo bug de buffer vira corrupção silenciosa. Mercado pequeno para quem está começando.

*Go* — coletor de lixo, concorrência fácil, curva rasa. O coletor introduz pausas, que num banco
aparecem como latência de cauda. Mercado backend brasileiro forte.

*Zig* — projeto interessante para o domínio, mas linguagem ainda instável e mercado praticamente
inexistente.

*Rust* — sem coletor de lixo, latência previsível, e o sistema de tipos transforma boa parte dos
bugs de gerenciamento de buffer em erro de compilação. `Drop` resolve o vazamento de pin
estruturalmente. Mercado menor que Go no Brasil, mas crescente e bem pago.

**Decisão.** Rust.

**O que se perde.** Curva de aprendizado íngreme somada a um domínio já difícil — dois riscos
simultâneos. Mitigação: a etapa 1 é o pager, o subsistema mais simples, deliberadamente escolhido
como lugar para pagar a curva. Tempo de compilação também dói em ciclo de iteração.

**Ganho colateral que pesou.** Meu portfólio é inteiro Python e JavaScript. Rust aqui é sinal de
alcance, não só de preferência.

---

## ADR-002 · Páginas de 4 KB

**Estado:** aceita

**Contexto.** O banco precisa de uma unidade fixa de E/S.

**Alternativas.** 512 B é o setor tradicional, pequeno demais: fanout baixo, árvore alta.
8 KB é o padrão do Postgres, bom para varredura, ruim para acesso aleatório.
16 KB é o padrão do InnoDB, ainda mais enviesado para varredura.
Tamanho configurável multiplica o espaço de teste sem ganho proporcional.

**Decisão.** 4096 bytes, fixo, gravado no cabeçalho para validação futura.

**Justificativa.** Casa com o tamanho de bloco típico de ext4, NTFS e APFS, e com o setor físico
dos SSDs modernos. Uma página suja é uma escrita, não duas. Também casa com o tamanho de página de
memória, o que abre a porta para `mmap` mais tarde.

**O que se perde.** Cargas de varredura pura iriam melhor com páginas maiores. Não é o alvo.

---

## ADR-003 · Um único escritor

**Estado:** aceita

**Contexto.** Escritores concorrentes exigem travamento hierárquico, detecção de deadlock e
escalonamento de travas. É um subsistema grande.

**Alternativas.** Travamento em nível de linha, com grafo de espera e detecção de ciclo — o modelo
completo, e um projeto inteiro. Travamento no nível da tabela — meio-termo que ainda exige toda a
infraestrutura de deadlock. Escritor único — serializa escrita por um mutex do banco.

**Decisão.** Um escritor, muitos leitores. Leitores nunca bloqueiam, graças ao MVCC.

**Justificativa.** Remove uma classe inteira de problemas e libera esse tempo para recovery, que é
onde está o aprendizado real. É também o modelo do SQLite, que roda em bilhões de dispositivos.

**O que se perde.** A taxa de escrita não escala com o número de núcleos. Para a carga alvo — um
banco embutido — isso é aceitável e está declarado no README.

---

## ADR-004 · MVCC em vez de travamento em duas fases

**Estado:** aceita

**Contexto.** Leitores e escritores precisam coexistir sem que um espere pelo outro.

**Alternativas.** *2PL* é mais simples de implementar e chega a serializable de graça, mas leitor
bloqueia escritor e vice-versa, e exige detecção de deadlock mesmo com escritor único. *MVCC* não
bloqueia leitura, mas exige cadeia de versões, regra de visibilidade e coleta de lixo. *MVCC com
serializable snapshot isolation* fecha o write skew ao custo de rastrear dependências de leitura
e escrita em um grafo.

**Decisão.** MVCC com snapshot isolation, sem SSI.

**Justificativa.** É o modelo do Postgres, o que torna o aprendizado transferível. A regra de
visibilidade é um pedaço de lógica pequeno e altamente testável. E as anomalias que ele previne e
as que não previne são mensuráveis, o que vira uma seção de resultado no README.

**O que se perde.** Write skew acontece. Está
[documentado explicitamente](07-mvcc.md#write-skew-e-por-que-ele-fica) e tem teste que o
reproduz de propósito. Um banco que declara o que não faz é mais confiável que um que promete
serializabilidade e entrega snapshot isolation — que é, por sinal, o que o Oracle faz.

---

## ADR-005 · Logging fisiológico

**Estado:** aceita

**Contexto.** O WAL precisa registrar alterações de um jeito que o redo consiga reaplicar e o undo
consiga reverter.

**Alternativas.**

*Lógico* — registra a operação ("inseriu a chave 42"). Compacto, mas o redo precisa reexecutar a
operação, e reexecutar um split de B+Tree durante recovery produz a divergência mais difícil de
depurar que existe.

*Físico de página inteira* — registra os 4 KB resultantes. Trivialmente idempotente, mas gasta
4 KB de log para alterar um byte.

*Fisiológico* — registra a imagem antiga e a nova de uma faixa de bytes dentro de uma página
identificada. Lógico entre páginas, físico dentro da página.

**Decisão.** Fisiológico, com imagem antiga e nova.

**Justificativa.** Idempotência sem custo de página inteira. Aplicar a imagem nova duas vezes tem o
mesmo efeito de aplicar uma, o que é o requisito absoluto do redo. A imagem antiga dá o undo de
graça, o que habilita a política *steal*.

**O que se perde.** Volume de log maior que o lógico. Um `UPDATE` que muda 8 bytes escreve 16
bytes de imagem mais 36 de cabeçalho. Aceitável, e o log é sequencial.

---

## ADR-006 · Codificação de chave que preserva ordem

**Estado:** aceita

**Contexto.** A B+Tree precisa comparar chaves. Ou ela conhece os tipos, ou as chaves chegam
codificadas de forma que a comparação byte a byte já dê a ordem certa.

**Alternativas.** Passar um comparador por tipo para a árvore acopla índice a sistema de tipos e
custa uma chamada indireta por comparação, no caminho mais quente que existe. Codificar as chaves
de forma que `memcmp` baste concentra a complexidade em uma função pura e testável, ao custo de
uma etapa de codificação e de a chave codificada não ser legível.

**Decisão.** Codificação que preserva ordem, detalhada em
[02](02-formato-de-arquivo.md#codificação-de-chave-que-preserva-ordem).

**Justificativa.** A B+Tree fica completamente agnóstica a tipo — só entende bytes. Comparação é
`memcmp`, que o processador executa vetorizado. Chave composta é concatenação, sem caso especial.

**O que se perde.** Depurar exige decodificar a chave para lê-la. Mitigação: uma função
`decode_key_for_debug` desde o primeiro dia.

---

## ADR-007 · Planner baseado em regras

**Estado:** aceita

**Contexto.** O planner precisa escolher entre `SeqScan` e `IndexScan`, e entre estratégias de
junção.

**Alternativas.** *Baseado em custo* é o que bancos reais usam: coleta estatísticas, estima
cardinalidade, enumera planos e escolhe o mais barato. Exige histogramas, modelo de custo,
enumeração — um projeto inteiro, e sem estatísticas boas ele escolhe pior que regras simples.
*Baseado em regras* aplica heurísticas fixas em ordem: previsível, testável, e explicável em uma
página.

**Decisão.** Baseado em regras, com as seis regras listadas em [06](06-sql.md#planner).

**Justificativa.** O objetivo do projeto é armazenamento e durabilidade, não otimização de
consulta. Regras fixas dão planos razoáveis com uma fração do esforço, e o `EXPLAIN` torna cada
decisão inspecionável.

**O que se perde.** Consultas onde a escolha certa depende de cardinalidade real vão receber plano
ruim. Fica registrado como trabalho futuro, não como omissão.

---

## ADR-008 · Catálogo em tabelas do próprio banco

**Estado:** aceita

**Contexto.** O schema precisa morar em algum lugar.

**Alternativas.** Um arquivo JSON ao lado do `.lastro` seria simples de inspecionar, mas DDL
deixaria de ser transacional: um `CREATE TABLE` que cai no meio deixaria arquivo e banco
divergentes. Uma região reservada com formato próprio exigiria código de serialização,
versionamento e recovery separados. Tabelas normais do próprio banco herdam tudo que já existe.

**Decisão.** `lastro_tables`, `lastro_columns` e `lastro_indexes`, tabelas comuns com ids fixos.
`catalog_root` na página 0 é o único ponteiro de entrada.

**Justificativa.** DDL vira transação normal, com WAL, redo e undo de graça. Não existe código
especial para tornar DDL atômico, que é uma fonte clássica de bugs. E o banco descrever a si mesmo
é elegante o bastante para valer a seção no README.

**O que se perde.** Uma dependência circular na inicialização: para ler o catálogo é preciso saber
o schema do catálogo. Resolvida com o schema das três tabelas internas embutido em constantes no
código, e é a solução que o SQLite usa com `sqlite_master`.

---

## ADR-009 · Sem `mmap`

**Estado:** aceita

**Contexto.** Mapear o arquivo em memória eliminaria o buffer pool e as cópias.

**Alternativas.** *`mmap`* delega paginação ao sistema operacional. É rápido e simples de começar.
Mas o controle de quando a página vai ao disco se perde, e sem esse controle **a regra WAL não
pode ser garantida** — o sistema pode escrever uma página suja a qualquer instante, antes do log
correspondente. Além disso, erro de E/S vira `SIGBUS` em vez de `Result`, e o famoso artigo *Are
You Sure You Want to Use MMAP in Your Database Management System?* documenta o resto dos
problemas.

**Decisão.** `pread` e `pwrite` explícitos, com buffer pool próprio.

**Justificativa.** Controle total sobre quando cada página vai ao disco é pré-requisito da regra
WAL, que é a tese do projeto. Erros viram `Result`, tratáveis. E implementar o buffer pool é parte
do que se quer aprender.

**O que se perde.** Uma cópia a mais por leitura, e o trabalho de escrever a substituição de
páginas. Ambos são custo aceitável, e o segundo é o objetivo.

---

## ADR-010 · Rejeitadas

Registradas para não voltarem como ideia nova daqui a três meses.

**Storage LSM-tree em vez de B+Tree.** Escrita mais rápida, e é o que RocksDB e Cassandra usam. Mas
compactação é um subsistema grande, range scan exige merge de vários níveis, e o modelo se afasta
do banco relacional clássico, que é o que se quer entender. Boa ideia para um segundo projeto.

**Compilação de consulta para bytecode.** É o que o SQLite faz, com ganho real. Mas esconde a
semântica atrás de mais uma camada justamente enquanto a semântica está sendo definida.

**Cliente-servidor com protocolo de rede.** Deslocaria o foco para serialização, pool de conexão e
autenticação — nada disso ensina algo sobre armazenamento.

**Compatibilidade com o protocolo do Postgres.** Tentador, porque permitiria usar `psql` como
cliente. Mas exigiria implementar bem mais SQL do que o escopo pede, só para o `psql` não quebrar
nas consultas de introspecção que ele dispara ao conectar.

---

Anterior: [10 · Glossário](10-glossario.md) · [↑ README](../../README.md)
