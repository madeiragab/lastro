-- Leitor não bloqueia escritor, escritor não bloqueia leitor. Para isso
-- cada linha guarda versões, e cada versão sabe quem a criou e quem a
-- removeu. Nada é sobrescrito no lugar.

CREATE TABLE gado (
  id     INTEGER PRIMARY KEY,
  brinco TEXT NOT NULL,
  peso   REAL
);

INSERT INTO gado VALUES
  (1, 'BR-0001', 400.0),
  (2, 'BR-0002', 410.0),
  (3, 'BR-0003', 420.0);

-- Cada UPDATE escreve uma versão nova e marca a antiga como removida.
-- A antiga continua no arquivo: alguém pode estar lendo.
UPDATE gado SET peso = 441.0 WHERE id = 1;
UPDATE gado SET peso = 452.0 WHERE id = 1;
UPDATE gado SET peso = 463.0 WHERE id = 1;
DELETE FROM gado WHERE id = 3;

-- A consulta só enxerga a versão que o snapshot dela admite.
SELECT id, brinco, peso FROM gado;

-- E aqui o espaço das versões que ninguém mais alcança volta. O número
-- é quantas versões e entradas de índice foram embora.
VACUUM gado;

SELECT id, brinco, peso FROM gado;

-- Dentro de uma transação, o horizonte é a própria transação — então uma
-- versão que ela mesma removeu NÃO é varrida, porque ela ainda pode
-- desistir. É o mesmo motivo pelo qual uma transação longa segura o
-- horizonte para trás e impede a limpeza. No Postgres isso se chama
-- bloat, e causa metade dos incidentes de produção com o banco.
BEGIN;
DELETE FROM gado WHERE id = 2;
VACUUM gado;
ROLLBACK;

SELECT id, brinco, peso FROM gado;
