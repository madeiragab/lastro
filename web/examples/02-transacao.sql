-- Ou a transação inteira aconteceu, ou nenhuma parte dela aconteceu.
-- Nunca metade. É a pergunta que move o projeto.

CREATE TABLE conta (
  id    INTEGER PRIMARY KEY,
  dono  TEXT NOT NULL,
  saldo INTEGER
);

INSERT INTO conta VALUES (1, 'ana', 1000), (2, 'bruno', 1000);
SELECT dono, saldo FROM conta;

-- Uma transferência que dá errado no meio do caminho.
BEGIN;
UPDATE conta SET saldo = saldo - 300 WHERE dono = 'ana';
UPDATE conta SET saldo = saldo + 300 WHERE dono = 'bruno';

-- Dentro da transação, ela já enxerga o próprio trabalho.
SELECT dono, saldo FROM conta;

-- E aqui a transferência é abandonada.
ROLLBACK;

-- Nada aconteceu. Nem metade.
SELECT dono, saldo FROM conta;

-- Agora a mesma coisa, terminando bem.
BEGIN;
UPDATE conta SET saldo = saldo - 300 WHERE dono = 'ana';
UPDATE conta SET saldo = saldo + 300 WHERE dono = 'bruno';
COMMIT;

SELECT dono, saldo FROM conta;
