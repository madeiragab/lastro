-- Um rebanho, que é o exemplo que atravessa a documentação inteira.
-- Ctrl+Enter roda. O arquivo do banco vive na memória da sua aba.

CREATE TABLE gado (
  id     INTEGER PRIMARY KEY,
  brinco TEXT NOT NULL,
  peso   REAL,
  ativo  BOOLEAN
);

INSERT INTO gado VALUES
  (1, 'BR-0001', 431.5, TRUE),
  (2, 'BR-0002', 388.0, TRUE),
  (3, 'BR-0003', 502.25, FALSE),
  (4, 'BR-0004', 455.0, TRUE),
  (5, 'BR-0005', 377.5, TRUE);

-- O id é a chave primária, então a tabela É a árvore: as linhas já
-- saem em ordem de chave sem ninguém ter mandado ordenar.
SELECT id, brinco, peso FROM gado;

-- Filtro, ordenação e limite.
SELECT brinco, peso FROM gado
 WHERE peso > 400 AND ativo
 ORDER BY peso DESC
 LIMIT 3;

-- NULL não é falso: ele não passa no filtro, e também não passa na negação.
INSERT INTO gado VALUES (6, 'BR-0006', NULL, TRUE);
SELECT brinco FROM gado WHERE peso > 400;
SELECT brinco FROM gado WHERE peso <= 400;
