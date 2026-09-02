// Drives the real lastro binary, compiled to wasm32-wasip1, from the page.
//
// There is no server. The engine runs in the tab, and the "disk" underneath it
// is a Map held in this module: the same one is handed to every run, which is
// what makes the database survive from one statement to the next.

import {
  WASI,
  File,
  OpenFile,
  ConsoleStdout,
  PreopenDirectory,
} from "./vendor/index.js";

const DB_DIR = "/db";
const DB_FILE = "demo.lastro";
const DB_PATH = `${DB_DIR}/${DB_FILE}`;

const sqlBox = document.getElementById("sql");
const outBox = document.getElementById("out");
const runButton = document.getElementById("run");
const resetButton = document.getElementById("reset");
const stateLine = document.getElementById("state");
const exampleBar = document.getElementById("examples");

/** The virtual disk. Shared by every run, so writes persist across them. */
let disk = new Map();

/** The compiled engine, fetched once and instantiated per run. */
let engine = null;

const EXAMPLES = [
  { id: "01-primeiros-passos", label: "Primeiros passos" },
  { id: "02-transacao", label: "Transação e rollback" },
  { id: "03-indice-e-plano", label: "Índice e EXPLAIN" },
  { id: "04-mvcc-e-vacuum", label: "MVCC e VACUUM" },
];

// -- running ---------------------------------------------------------------

/**
 * Runs one batch of statements against the virtual disk.
 *
 * The CLI takes its statements as an argument and prints to stdout, so it needs
 * no changes to be driven from here: argv in, text out.
 */
async function run(statements) {
  const lines = [];
  const collect = (line) => lines.push(line);

  const fds = [
    new OpenFile(new File([])), // stdin, never read
    ConsoleStdout.lineBuffered(collect),
    ConsoleStdout.lineBuffered(collect),
    // The same Map every time. That is the persistence.
    new PreopenDirectory(DB_DIR, disk),
    // Somewhere for the external sort to spill, if a query ever asks it to.
    new PreopenDirectory("/tmp", new Map()),
  ];

  const wasi = new WASI(["lastro-cli", "sql", DB_PATH, statements], [], fds);
  const instance = await WebAssembly.instantiate(engine, {
    wasi_snapshot_preview1: wasi.wasiImport,
  });

  let code = 0;
  try {
    code = wasi.start(instance) ?? 0;
  } catch (error) {
    // A non-zero exit arrives as a thrown WASIProcExit, which is how the CLI
    // reports a bad statement. The message it printed is already in `lines`.
    if (error && typeof error.code === "number") {
      code = error.code;
    } else {
      throw error;
    }
  }

  return { text: lines.join("\n"), code };
}

async function onRun() {
  const statements = sqlBox.value.trim();
  if (!statements) {
    return;
  }

  runButton.disabled = true;
  runButton.textContent = "Rodando…";
  const started = performance.now();

  try {
    const { text, code } = await run(statements);
    const took = (performance.now() - started).toFixed(1);
    print(text || "(sem saída)", code === 0 ? "ok" : "erro");
    setState(
      code === 0
        ? `pronto em ${took} ms · banco com ${size()} no arquivo`
        : `terminou com código ${code} em ${took} ms`,
    );
  } catch (error) {
    // A panic inside the engine unwinds to here rather than to nowhere.
    print(`o motor parou: ${error}`, "erro");
    setState("o banco pode ter ficado num estado estranho — use “Apagar o banco”");
  } finally {
    runButton.disabled = false;
    runButton.textContent = "Rodar";
  }
}

// -- the page --------------------------------------------------------------

function print(text, kind) {
  outBox.textContent = text;
  outBox.dataset.kind = kind;
}

function setState(text) {
  stateLine.textContent = text;
}

/** How big the virtual database file is, in something readable. */
function size() {
  const file = disk.get(DB_FILE);
  if (!file) {
    return "0 bytes";
  }
  const bytes = file.data.byteLength;
  return bytes < 1024 ? `${bytes} bytes` : `${(bytes / 1024).toFixed(1)} KB`;
}

async function loadExample(id) {
  const response = await fetch(`./examples/${id}.sql`);
  if (!response.ok) {
    throw new Error(`exemplo ${id} não carregou`);
  }
  sqlBox.value = (await response.text()).trim();
  outBox.textContent = "";
  setState("");
}

function buildExampleBar() {
  for (const [index, example] of EXAMPLES.entries()) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "chip";
    button.textContent = example.label;
    button.addEventListener("click", async () => {
      for (const other of exampleBar.children) {
        other.classList.remove("on");
      }
      button.classList.add("on");
      // Each example assumes a clean database, so it gets one.
      disk = new Map();
      await loadExample(example.id);
    });
    if (index === 0) {
      button.classList.add("on");
    }
    exampleBar.append(button);
  }
}

function onReset() {
  disk = new Map();
  print("banco apagado. o próximo comando cria um arquivo novo.", "ok");
  setState("");
}

// -- start -----------------------------------------------------------------

async function start() {
  buildExampleBar();

  try {
    await loadExample(EXAMPLES[0].id);
  } catch {
    sqlBox.value = "SELECT 1;";
  }

  try {
    // Compiled once; every run instantiates it fresh, which is what makes each
    // run a clean process against a dirty disk.
    engine = await WebAssembly.compileStreaming(fetch("./lastro.wasm"));
  } catch (error) {
    runButton.textContent = "O motor não carregou";
    print(
      `Não consegui carregar o WebAssembly: ${error}\n\n` +
        "Isto precisa de um navegador com suporte a WebAssembly e a módulos ES.\n" +
        "O código continua disponível em github.com/madeiragab/lastro.",
      "erro",
    );
    return;
  }

  runButton.disabled = false;
  runButton.textContent = "Rodar";
  setState("motor carregado");

  runButton.addEventListener("click", onRun);
  resetButton.addEventListener("click", onReset);

  // Ctrl+Enter runs, which is what anybody who has used a SQL console expects.
  sqlBox.addEventListener("keydown", (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key === "Enter") {
      event.preventDefault();
      onRun();
    }
  });
}

start();
