[Português](10-glossario.md) · [English](../en/10-glossary.md) · [↑ README](../../README.md)

# 10 · Glossário

Vocabulário usado na documentação. Definições curtas, sem circularidade.

---

**ARIES** — algoritmo de recovery publicado por Mohan e outros em 1992. Três fases: análise, redo,
undo. Base de praticamente todo banco relacional em uso. Ver [05](05-wal-recovery.md).

**B+Tree** — árvore de busca balanceada em que os valores ficam só nas folhas e os nós internos
guardam apenas separadores. Folhas ligadas em lista para varredura sequencial. Ver [04](04-btree.md).

**Buffer pool** — cache em memória de páginas do disco, com tamanho fixo e política de
substituição. Ver [03](03-pager.md#buffer-pool).

**Célula** — um registro de tamanho variável dentro de uma slotted page. Ver
[02](02-formato-de-arquivo.md#células).

**Checkpoint** — marca no log que limita onde o recovery precisa começar. *Fuzzy* quer dizer que
o banco continua operando durante ele.

**CLR** *(compensation log record)* — registro que documenta um undo já executado. Nunca é
desfeito, só refeito. É o que permite que uma queda durante o recovery seja segura.

**Dirty page** — página modificada em memória cuja versão em disco está desatualizada.

**Dirty read** — ler dado de uma transação que ainda não commitou. Prevenido aqui.

**Fanout** — número de filhos de um nó interno. Fanout alto significa árvore baixa, e altura é
contagem de leituras de disco.

**Freelist** — lista ligada de páginas alocadas e depois liberadas, disponíveis para reuso.

**`fsync`** — chamada de sistema que força os dados de um arquivo a chegarem ao meio físico.
Retorna só depois disso — em teoria; alguns dispositivos mentem.

**Fuzzy checkpoint** — checkpoint que não bloqueia o banco. Ver [05](05-wal-recovery.md#checkpoint).

**Horizonte** — o menor `xmin` entre os snapshots ativos. Nenhuma versão criada acima dele pode
ser coletada. Ver [07](07-mvcc.md#coleta-de-versões-mortas).

**Idempotente** — aplicar duas vezes tem o mesmo efeito de aplicar uma. Propriedade obrigatória do
redo.

**Isolamento** — quanto uma transação enxerga do trabalho em curso das outras. Aqui: snapshot
isolation.

**Lost update** — duas transações leem o mesmo valor, ambas escrevem, e uma sobrescreve a outra
silenciosamente. Prevenido aqui por *first-updater-wins*.

**LSN** *(log sequence number)* — identificador monotônico de um registro de log. Aqui é o próprio
offset do registro no arquivo.

**`memcmp`** — comparação byte a byte. Toda a estratégia de codificação de chave existe para que
`memcmp` produza a ordem lógica correta. Ver
[02](02-formato-de-arquivo.md#codificação-de-chave-que-preserva-ordem).

**MVCC** *(multiversion concurrency control)* — escrever cria uma versão nova em vez de
sobrescrever, para que leitores nunca bloqueiem escritores. Ver [07](07-mvcc.md).

**No-force** — política em que páginas sujas não precisam ir ao disco no commit. Exige redo.

**Overflow page** — página extra que guarda o excedente de uma célula grande demais. Ver
[02](02-formato-de-arquivo.md#overflow).

**Pager** — a camada mais baixa. Lê e escreve páginas, aloca e libera. Ver [03](03-pager.md).

**Página** — unidade de E/S do banco. Aqui, 4096 bytes.

**Phantom read** — a mesma consulta com predicado devolve linhas novas quando repetida na mesma
transação. Prevenido aqui.

**Pin** — marcar uma página como em uso para que o buffer pool não a despeje. Todo pin precisa de
um unpin correspondente.

**`pread` e `pwrite`** — leitura e escrita posicionais, que recebem o offset como argumento em vez
de usar o cursor do arquivo.

**Política clock** — aproximação barata de LRU, usando um bit de referência e um ponteiro
circular. Ver [03](03-pager.md#política-clock).

**Redo** — reaplicar alterações registradas no log. Segunda fase do recovery.

**Regra WAL** — o registro de log vai para o disco antes da página que ele descreve. A regra que
define o projeto.

**Repeatable read** — a mesma consulta devolve o mesmo resultado durante toda a transação.
Consequência automática de o snapshot ser imutável.

**RowId** — o par `(page_id, slot_id)` que identifica uma tupla no heap. Estável mesmo após
compactação da página.

**Serializable** — nível de isolamento em que a execução concorrente equivale a alguma execução
sequencial. **Não implementado aqui**, e declarado como tal.

**Slot** — entrada de 4 bytes com offset e comprimento, apontando para uma célula. O slot é o
endereço estável; a célula pode se mover.

**Slotted page** — layout com slots crescendo do início e células do fim, e espaço livre no meio.
Ver [02](02-formato-de-arquivo.md#slotted-page).

**Snapshot** — fotografia do estado de concorrência no início de uma transação. Determina o que ela
enxerga.

**Split** — divisão de uma página cheia em duas, com promoção de um separador ao pai. Ver
[04](04-btree.md#inserção-e-split).

**Steal** — política que permite despejar página suja de transação não commitada. Exige undo.

**Tombstone** — marca de remoção que deixa os bytes no lugar. Alternativa ao merge imediato.

**Undo** — desfazer alterações de transações que não commitaram. Terceira fase do recovery.

**Vacuum** — coleta das versões que nenhum snapshot ativo ou futuro pode enxergar. Ver
[07](07-mvcc.md#coleta-de-versões-mortas).

**Varint** — inteiro de comprimento variável, 7 bits úteis por byte. Ver
[02](02-formato-de-arquivo.md#varint).

**Volcano** — modelo de execução em que cada operador expõe um `next()` que puxa uma tupla do
operador abaixo. Também chamado modelo iterator. Ver [06](06-sql.md#executor).

**WAL** *(write-ahead log)* — log sequencial escrito antes das páginas de dados. Ver
[05](05-wal-recovery.md).

**Write skew** — duas transações leem um estado compartilhado, escrevem em linhas diferentes, e
juntas violam uma invariante que cada uma sozinha respeitaria. **Não prevenido aqui**, por escolha
declarada. Ver [07](07-mvcc.md#write-skew-e-por-que-ele-fica).

**`xmin` e `xmax`** — os txids que criaram e removeram uma versão de tupla. Base da regra de
visibilidade.

---

Anterior: [09 · Roadmap](09-roadmap.md) · Próximo: [ADR](adr.md)
