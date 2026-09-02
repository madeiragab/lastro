# Diário de bugs

**Português** · [English](POSTMORTEM.en.md)

O [README](README.md) conta o que o `lastro` faz quando funciona. Este documento conta as vezes
em que não funcionou.

Está aqui porque é a parte que mais ensinou. Um banco de dados é fácil de escrever e difícil de
acreditar: quase todo erro sério deste projeto passou pelos testes existentes, não devolveu erro
nenhum, e só apareceu quando alguma coisa de fora — um fuzzer, um corpus, outro sistema
operacional — perguntou algo que eu não tinha pensado em perguntar.

Cada entrada abaixo é um commit real. O sintoma vem antes da causa, na ordem em que apareceu.

---

## 1 · O log recomeçava do zero depois do checkpoint

`17e553f` — `src/wal/`

**O sintoma.** Nenhum. É o que torna este o pior da lista.

**A causa.** Depois de um checkpoint, o WAL é truncado — os registros já estão aplicados no
arquivo de dados, não servem mais para nada. Mas a numeração recomeçava do zero junto com o
arquivo.

O LSN não é um contador decorativo. Toda página em disco carrega o LSN da última alteração que a
tocou, e o redo usa essa comparação para decidir se um registro do log já está aplicado ou não:
se o LSN da página é maior, pula. Um log renumerado a partir do zero parece **mais antigo que
todas as páginas do disco**. O recovery lê o log inteiro, conclui que cada registro já foi
aplicado, e pula tudo — em silêncio, sem erro, sem aviso.

O resultado é a pior falha que um banco pode ter: uma transação confirmada, com `COMMIT`
retornado ao cliente, desaparece depois de uma queda. E o recovery relata sucesso.

**A correção.** A numeração continua atravessando a truncagem, e o ponto onde ela continua passa
a ser durável — `last_checkpoint_lsn` na página de metadados, sincronizado.

Junto veio uma questão de ordem que eu tinha errado pelo mesmo motivo: só depois de o log estar
vazio é seguro escrever cabeçalhos de freelist por cima das páginas que transações confirmadas
liberaram. Antes disso ainda existe algo para reaplicar em cima delas.

**O que ficou.** Estado durável e estado em memória precisam ser conferidos separadamente. O
crash fuzzer só pega isto porque reabre o banco de verdade a cada corte, em vez de inspecionar
as estruturas que já estão carregadas.

---

## 2 · `ORDER BY 1` ordenava por nada

`bc257e0` — `src/sql/plan.rs`

**O sintoma.** Todo valor certo, a ordem errada, e nenhum erro em lugar nenhum.

**A causa.** `ORDER BY 1` significa "a primeira coluna da saída". O planner lia literalmente:
ordenar pelo número 1. Ordenar por uma constante não faz nada, então as linhas voltavam na
ordem em que a varredura as produziu. Que às vezes é a ordem certa — o que é pior do que estar
sempre errado, porque some do teste feito à mão.

**Como apareceu.** Não foi um teste meu. Foi o corpus do `sqllogictest`, do próprio SQLite, que
usa a forma ordinal em **43 lugares**. Nenhum dos meus testes de parser usava, porque eu escrevo
`ORDER BY nome`, e eu era quem escrevia os testes.

**A correção.** `bind_order_key` resolve o literal inteiro contra as colunas da projeção, e um
ordinal fora da faixa vira erro em vez de silêncio:

```
ORDER BY 7 names an output column, and there are 3
```

**O que ficou.** A razão de rodar um corpus de terceiro não é cobertura. É que o corpus não
compartilha os meus pontos cegos. Meus testes verificavam o que eu tinha pensado em implementar,
e é exatamente essa a interseção onde bug nenhum mora.

---

## 3 · `pread` instável na build do navegador

`5c97a76` — `src/storage/pager.rs`

**O sintoma.** A build `wasm32-wasip1` não compilava no toolchain estável.

**A causa.** `std::os::wasi::fs::FileExt` — a entrada/saída posicionada, ler em um deslocamento
sem mexer no cursor do arquivo — existe no WASI, mas está atrás de um *feature* instável. As
builds Unix e Windows usam `pread`/`pwrite` das respectivas extensões, e a de WASI não tinha
para onde ir.

**A correção.** No WASI, buscar e depois transferir.

Isso é uma troca de correção por portabilidade, não uma equivalência, e o motivo de valer aqui
está escrito ao lado dela: o navegador roda **uma thread**, e cada chamada busca imediatamente
antes de transferir. Ninguém pode mover o cursor no meio. Em um host com threads seria uma
corrida — que é precisamente por que as outras duas plataformas não fazem assim.

**O que ficou.** Correção que depende da plataforma precisa do argumento escrito junto do código,
não do lado de fora. Sem o comentário, o padrão é copiado para onde ele não vale, e a corrida
que aparece não tem nada que a explique.

---

## 4 · Um acento no comentário corrompia a memória

`e1603fa` — `src/bin/lastro-cli.rs`, `web/app.js`

**O sintoma.** O motor reclamando de uma coluna chamada `C` que ninguém tinha escrito.

**A causa.** A demo do navegador entregava o SQL como argumento de linha de comando. O shim WASI
dimensiona o buffer de argumentos com `arg.length` do JavaScript, que conta **unidades UTF-16**,
e depois preenche esse buffer com os **bytes UTF-8** da string.

Enquanto tudo é ASCII, os dois números batem e nada acontece. Um único caractere acentuado —
em um comentário, inclusive — ocupa um UTF-16 e dois bytes UTF-8. O buffer fica um byte curto,
a escrita passa do fim, e corrompe o que estiver ao lado na memória. O `C` fantasma era outro
argumento sobrescrito.

**A correção.** `lastro-cli sql <arquivo> -` lê os comandos da entrada padrão, e a demo passa a
usar isso. Descritor de arquivo carrega bytes e não tem a divergência. É também a convenção de
sempre, e a única forma de entregar um script que não cabe em um argumento.

**O que ficou.** Este não era um bug meu — era do shim. Mas a mensagem de erro apontava para o
meu parser, e é lá que eu procurei primeiro. Quando o sintoma não faz sentido nenhum na camada
onde ele aparece, a camada errada é a que se está olhando.

---

## 5 · Uma invariante que a especificação afirmava e o `proptest` derrubou — duas vezes

`docs/pt/04-btree.md`

Não é um bug no código. É um erro na especificação, e vale mais do que os quatro de cima juntos.

A especificação afirmava um piso de ocupação por nó da B+Tree. Duas formulações foram tentadas,
e nenhuma sobreviveu:

- **"todo nó tem pelo menos 40% de ocupação"** cai porque uma única célula pode ocupar um terço
  da página. Uma divisão equilibrada pode deixar as duas metades abaixo do piso sem que exista
  nada de errado.
- **"nenhum par de irmãos adjacentes cabe junto em uma página"** cai na **inserção**, não na
  remoção. Quando um nó cheio racha ao meio, cada metade fica com meia página, e uma delas pode
  passar a caber junto com o vizinho intocado ao lado — vizinho ao qual ela nunca precisou caber
  antes. O caso mínimo que o `proptest` reduziu não tem nenhuma remoção.

Então o fator de preenchimento **não é afirmado, é medido** — `BTree::stats` — e os testes
afirmam sobre a medida.

**O que ficou.** Um limite que só vale para registros de tamanho fixo não é um limite. E quando
o teste discorda da especificação, a hipótese padrão não é que o teste está errado.

---

## O padrão

Quatro dos cinco não devolveram erro nenhum. Dois foram encontrados por algo escrito por outra
pessoa — o corpus do SQLite e o redutor do `proptest`. Um só existia em uma plataforma. Nenhum
apareceu executando o programa à mão.

É por isso que as provas deste repositório são do formato que são, e por que o denominador anda
sempre junto do numerador em [08 · Testes](docs/pt/08-testes.md): o número que importa não é
quantos testes passam, é quantas perguntas que eu não teria feito alguém fez por mim.
