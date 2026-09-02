-- Planner que não mostra o que decidiu é planner que não dá para conferir.
-- Por isso EXPLAIN existe desde a primeira versão da camada SQL.

CREATE TABLE gado (
  id     INTEGER PRIMARY KEY,
  brinco TEXT NOT NULL,
  peso   REAL,
  lote   INTEGER
);

INSERT INTO gado VALUES
  (1, 'BR-0001', 431.5, 7),
  (2, 'BR-0002', 388.0, 7),
  (3, 'BR-0003', 502.25, 9),
  (4, 'BR-0004', 455.0, 9),
  (5, 'BR-0005', 377.5, 7);

-- Sem índice em `lote`, só resta varrer a tabela.
EXPLAIN SELECT brinco FROM gado WHERE lote = 9;

CREATE INDEX gado_lote ON gado (lote);

-- Com índice, o plano muda. Repare que o predicado CONTINUA acima da
-- varredura por índice: a entrada do índice não é removida quando a
-- versão que ela apontava é superada, então reconferir em cima é o que
-- torna uma entrada obsoleta inofensiva em vez de errada.
EXPLAIN SELECT brinco FROM gado WHERE lote = 9;
SELECT brinco FROM gado WHERE lote = 9;

-- A chave primária é a própria árvore, então isto é uma descida, não uma busca.
EXPLAIN SELECT peso FROM gado WHERE id = 3;

-- Ordenar pela chave primária ascendente é trabalho que já aconteceu:
-- a varredura já entrega nessa ordem, e o planner tira o Sort.
EXPLAIN SELECT brinco FROM gado ORDER BY id;
EXPLAIN SELECT brinco FROM gado ORDER BY peso DESC;

-- DISTINCT fica acima da projeção e abaixo do limite, então o limite
-- conta linhas que sobreviveram ao colapso.
EXPLAIN SELECT DISTINCT lote FROM gado LIMIT 1;
SELECT DISTINCT lote FROM gado;
