// Cinder playground front end. No build step: plain ES modules, Monaco from a CDN
// (with a plain <textarea> fallback when the CDN is unreachable).
import { EXAMPLES } from './examples.js';

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

const MONACO_BASE = 'https://cdn.jsdelivr.net/npm/monaco-editor@0.52.2/min';
const COMPILE_DEBOUNCE_MS = 450;

const store = {
  get(key, fallback) {
    try { const v = localStorage.getItem('cinder.' + key); return v === null ? fallback : v; } catch { return fallback; }
  },
  set(key, value) {
    try { localStorage.setItem('cinder.' + key, value); } catch { /* storage may be blocked */ }
  },
};

const state = {
  opt: 0,
  emit: 'asm',
  dark: false,
  running: false,
  runEnabled: true,
  lineMap: [],
  srcToOut: new Map(),
  compileSeq: 0,
  compileCtl: null,
};

// ───────────────────────────── status and toasts ─────────────────────────────

function setStatus(text, cls = '') {
  const el = $('#status');
  el.textContent = text;
  el.className = cls;
}

let toastTimer = 0;
function toast(text) {
  const el = $('#toast');
  el.textContent = text;
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { el.hidden = true; }, 2600);
}

// ───────────────────────────── Monaco and the fallback editor ─────────────────────────────

function loadMonaco(timeoutMs = 9000) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve(null), timeoutMs);
    const done = (v) => { clearTimeout(timer); resolve(v); };
    const s = document.createElement('script');
    s.src = `${MONACO_BASE}/vs/loader.js`;
    s.onerror = () => done(null);
    s.onload = () => {
      try {
        window.require.config({ paths: { vs: `${MONACO_BASE}/vs` } });
        window.MonacoEnvironment = {
          getWorkerUrl: () => URL.createObjectURL(new Blob(
            [`self.MonacoEnvironment = { baseUrl: '${MONACO_BASE}/' }; importScripts('${MONACO_BASE}/vs/base/worker/workerMain.js');`],
            { type: 'text/javascript' })),
        };
        window.require(['vs/editor/editor.main'], () => done(window.monaco), () => done(null));
      } catch { done(null); }
    };
    document.head.appendChild(s);
  });
}

function registerLanguages(monaco) {
  monaco.languages.register({ id: 'att-asm' });
  monaco.languages.setMonarchTokensProvider('att-asm', {
    tokenizer: {
      root: [
        [/#.*$/, 'comment'],
        [/^\s*[.\w$@]+:/, 'label'],
        [/^\s*\.\w+/, 'directive'],
        [/"(?:[^"\\]|\\.)*"/, 'string'],
        [/%[a-z0-9]+/, 'register'],
        [/\$-?(?:0x[0-9a-fA-F]+|\d+)/, 'number'],
        [/-?(?:0x[0-9a-fA-F]+|\d+)(?=\()/, 'number'],
        [/\b(?:0x[0-9a-fA-F]+|\d+)\b/, 'number'],
        [/^\s*[a-z][a-z0-9]*/, 'mnemonic'],
        [/\.L[\w.]+/, 'label'],
      ],
    },
  });
  const irKeywords = ['define', 'declare', 'alloca', 'dynalloca', 'stacksave', 'stackrestore', 'load', 'store', 'volatile', 'memcpy', 'memset',
    'add', 'sub', 'mul', 'sdiv', 'udiv', 'srem', 'urem', 'and', 'or', 'xor', 'shl', 'lshr', 'ashr', 'fadd', 'fsub', 'fmul', 'fdiv', 'neg', 'not', 'fneg',
    'icmp', 'fcmp', 'zext', 'sext', 'trunc', 'sitofp', 'uitofp', 'fptosi', 'fptoui', 'fpext', 'fptrunc', 'ptrtoint', 'inttoptr', 'ptradd', 'select', 'phi',
    'call', 'tail', 'variadic', 'br', 'condbr', 'switch', 'ret', 'unreachable', 'trap', 'internal', 'default', 'byval', 'void'];
  monaco.languages.register({ id: 'cinder-ir' });
  monaco.languages.setMonarchTokensProvider('cinder-ir', {
    keywords: irKeywords,
    tokenizer: {
      root: [
        [/;.*$/, 'comment'],
        [/^[\w.]+:/, 'label'],
        [/%[\w.<>]+/, 'register'],
        [/@[\w.]+/, 'label'],
        [/\b(?:i8|i16|i32|i64|ptr|f32|f64)\b/, 'type'],
        [/-?\b(?:0x[0-9a-fA-F]+|\d+(?:\.\d+)?)\b/, 'number'],
        [/"(?:[^"\\]|\\.)*"/, 'string'],
        [/[a-z_][\w]*/, { cases: { '@keywords': 'mnemonic', '@default': 'identifier' } }],
      ],
    },
  });
  const rules = (dark) => [
    { token: 'comment', foreground: dark ? '7d8590' : '6a737d', fontStyle: 'italic' },
    { token: 'label', foreground: dark ? 'ffa657' : 'b35900', fontStyle: 'bold' },
    { token: 'directive', foreground: dark ? 'a5d6ff' : '0550ae' },
    { token: 'register', foreground: dark ? '7ee787' : '116329' },
    { token: 'number', foreground: dark ? '79c0ff' : '0550ae' },
    { token: 'mnemonic', foreground: dark ? 'ff7b72' : 'cf222e' },
    { token: 'type', foreground: dark ? 'd2a8ff' : '8250df' },
    { token: 'string', foreground: dark ? 'a5d6ff' : '0a3069' },
  ];
  monaco.editor.defineTheme('cinder-light', { base: 'vs', inherit: true, rules: rules(false), colors: {} });
  monaco.editor.defineTheme('cinder-dark', { base: 'vs-dark', inherit: true, rules: rules(true), colors: {} });
}

class MonacoEditor {
  constructor(monaco, host, { language, readOnly }) {
    this.monaco = monaco;
    this.editor = monaco.editor.create(host, {
      value: '',
      language,
      readOnly,
      automaticLayout: true,
      minimap: { enabled: false },
      fontSize: 13,
      fontLigatures: false,
      scrollBeyondLastLine: false,
      renderLineHighlight: readOnly ? 'none' : 'line',
      tabSize: 4,
      insertSpaces: true,
      glyphMargin: !readOnly,
      lineNumbersMinChars: 3,
      padding: { top: 6 },
      wordWrap: 'off',
      accessibilitySupport: 'auto',
      ariaLabel: readOnly ? 'Compiler output' : 'C source code',
    });
    this.decorations = this.editor.createDecorationsCollection([]);
  }
  get value() { return this.editor.getValue(); }
  set value(v) { this.editor.setValue(v); }
  setLanguage(id) { this.monaco.editor.setModelLanguage(this.editor.getModel(), id); }
  onChange(cb) { this.editor.onDidChangeModelContent(cb); }
  onCursorLine(cb) {
    let last = 0;
    this.editor.onDidChangeCursorPosition((e) => { if (e.position.lineNumber !== last) { last = e.position.lineNumber; cb(last); } });
  }
  onRun(cb) { this.editor.addCommand(this.monaco.KeyMod.CtrlCmd | this.monaco.KeyCode.Enter, cb); }
  setMarkers(markers) {
    this.monaco.editor.setModelMarkers(this.editor.getModel(), 'cinder', markers);
  }
  highlightLines(lines) {
    this.decorations.set(lines.map((n) => ({
      range: new this.monaco.Range(n, 1, n, 1),
      options: { isWholeLine: true, className: 'hl-link', linesDecorationsClassName: 'hl-link-margin' },
    })));
  }
  reveal(line) { this.editor.revealLineInCenterIfOutsideViewport(line); }
  goTo(line, col) {
    this.editor.setPosition({ lineNumber: line, column: col || 1 });
    this.editor.revealLineInCenter(line);
    this.editor.focus();
  }
  setDark(dark) { this.monaco.editor.setTheme(dark ? 'cinder-dark' : 'cinder-light'); }
}

/** Minimal stand-in used when Monaco cannot be loaded: no markers or line linking. */
class TextEditor {
  constructor(host, { readOnly }) {
    this.ta = document.createElement('textarea');
    this.ta.className = 'fallback';
    this.ta.spellcheck = false;
    this.ta.readOnly = readOnly;
    this.ta.setAttribute('aria-label', readOnly ? 'Compiler output' : 'C source code');
    host.appendChild(this.ta);
    this.ta.addEventListener('keydown', (e) => {
      if (e.key === 'Tab' && !readOnly) {
        e.preventDefault();
        const { selectionStart: s, selectionEnd: t } = this.ta;
        this.ta.setRangeText('    ', s, t, 'end');
        this.ta.dispatchEvent(new Event('input'));
      }
    });
  }
  get value() { return this.ta.value; }
  set value(v) { this.ta.value = v; }
  setLanguage() {}
  onChange(cb) { this.ta.addEventListener('input', cb); }
  onCursorLine(cb) {
    const emit = () => cb(this.ta.value.slice(0, this.ta.selectionStart).split('\n').length);
    this.ta.addEventListener('click', emit);
    this.ta.addEventListener('keyup', emit);
  }
  onRun(cb) { this.ta.addEventListener('keydown', (e) => { if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') { e.preventDefault(); cb(); } }); }
  setMarkers() {}
  highlightLines() {}
  reveal() {}
  goTo(line, col) {
    const lines = this.ta.value.split('\n');
    let pos = 0;
    for (let i = 0; i < line - 1 && i < lines.length; i++) pos += lines[i].length + 1;
    pos += Math.max(0, (col || 1) - 1);
    this.ta.focus();
    this.ta.setSelectionRange(pos, pos);
  }
  setDark() {}
}

let source;
let output;
let usingMonaco = false;

async function createEditors() {
  const monaco = await loadMonaco();
  usingMonaco = !!monaco;
  if (monaco) {
    registerLanguages(monaco);
    source = new MonacoEditor(monaco, $('#source-host'), { language: 'c', readOnly: false });
    output = new MonacoEditor(monaco, $('#output-host'), { language: 'att-asm', readOnly: true });
  } else {
    source = new TextEditor($('#source-host'), { readOnly: false });
    output = new TextEditor($('#output-host'), { readOnly: true });
    toast('Editor library unavailable (offline?): using a plain text editor');
  }
}

const OUTPUT_LANG = { asm: 'att-asm', ir: 'cinder-ir', ast: 'plaintext', hir: 'plaintext', pp: 'c' };

// ───────────────────────────── API ─────────────────────────────

async function api(path, body, signal) {
  const res = await fetch(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
    signal,
  });
  let data = null;
  try { data = await res.json(); } catch { /* not JSON */ }
  if (!res.ok) {
    const err = new Error((data && data.error) || `server error (HTTP ${res.status})`);
    err.status = res.status;
    throw err;
  }
  return data;
}

// ───────────────────────────── diagnostics ─────────────────────────────

const SEVERITY = { error: 8, fatal: 8, warning: 4, note: 2 };

function renderDiagnostics(list, extraText = '') {
  const box = $('#console-diag');
  box.textContent = '';
  const items = list || [];
  const errors = items.filter((d) => d.level === 'error' || d.level === 'fatal').length;
  const warnings = items.filter((d) => d.level === 'warning').length;
  if (!items.length && !extraText) {
    const p = document.createElement('div');
    p.className = 'diag-empty';
    p.textContent = 'No diagnostics.';
    box.appendChild(p);
  }
  for (const d of items) {
    const b = document.createElement('button');
    b.className = 'diag-item';
    b.type = 'button';
    const head = document.createElement('span');
    head.className = 'diag-head';
    head.textContent = `main.c:${d.line}:${d.col}`;
    const msg = document.createElement('span');
    msg.className = 'diag-msg';
    const sev = document.createElement('span');
    sev.className = 'diag-sev ' + d.level;
    sev.textContent = d.level;
    msg.append(sev, d.message + (d.flag ? ` [${d.flag}]` : ''));
    b.append(head, msg);
    for (const n of d.notes || []) {
      const note = document.createElement('span');
      note.className = 'diag-note';
      note.textContent = `note: ${n.message}${n.line ? ` (line ${n.line})` : ''}`;
      b.appendChild(note);
    }
    b.addEventListener('click', () => { source.goTo(d.line, d.col); if (document.body.dataset.view !== 'source') setView('source'); });
    box.appendChild(b);
  }
  if (extraText) {
    const pre = document.createElement('pre');
    pre.className = 'out-stderr';
    pre.textContent = extraText;
    box.appendChild(pre);
  }
  const badge = $('#diag-badge');
  const total = errors + warnings;
  badge.hidden = total === 0;
  badge.textContent = String(total);
  badge.classList.toggle('warn', errors === 0);
  if (usingMonaco) {
    source.setMarkers(items.filter((d) => d.level !== 'note').map((d) => ({
      severity: SEVERITY[d.level] || 4,
      message: d.message,
      code: d.flag || undefined,
      startLineNumber: d.line,
      startColumn: d.col,
      endLineNumber: d.endLine || d.line,
      endColumn: Math.max(d.endCol || d.col + 1, d.col + 1),
    })));
  }
  return { errors, warnings };
}

// ───────────────────────────── compile (output pane) ─────────────────────────────

function setLineMap(map) {
  state.lineMap = map || [];
  state.srcToOut = new Map();
  state.lineMap.forEach((src, i) => {
    if (!src) return;
    if (!state.srcToOut.has(src)) state.srcToOut.set(src, []);
    state.srcToOut.get(src).push(i + 1);
  });
  source.highlightLines([]);
  output.highlightLines([]);
  $('#map-hint').textContent = state.lineMap.some(Boolean)
    ? 'click a line to link it with the source'
    : 'this view has no source-line mapping';
}

let compileTimer = 0;
function scheduleCompile() {
  clearTimeout(compileTimer);
  compileTimer = setTimeout(compileNow, COMPILE_DEBOUNCE_MS);
  setStatus('Editing…');
}

async function compileNow() {
  clearTimeout(compileTimer);
  const seq = ++state.compileSeq;
  if (state.compileCtl) state.compileCtl.abort();
  const ctl = (state.compileCtl = new AbortController());
  const started = performance.now();
  setStatus('Compiling…');
  try {
    const r = await api('/api/compile', { code: source.value, optLevel: state.opt, emit: state.emit }, ctl.signal);
    if (seq !== state.compileSeq) return;
    output.setLanguage(OUTPUT_LANG[r.emit] || 'plaintext');
    if (r.ok || r.output) {
      output.value = r.output;
      setLineMap(r.lineMap);
    } else {
      output.value = '// Compilation failed: see the Diagnostics tab.\n';
      setLineMap([]);
    }
    const { errors, warnings } = renderDiagnostics(r.diagnostics, r.stderr);
    const ms = Math.round(performance.now() - started);
    if (!r.ok) {
      setStatus(r.timedOut ? 'The compiler timed out' : `${errors} error${errors === 1 ? '' : 's'}${warnings ? `, ${warnings} warning${warnings === 1 ? '' : 's'}` : ''}`, 'error');
    } else {
      setStatus(`Compiled in ${r.timeMs} ms${warnings ? ` · ${warnings} warning${warnings === 1 ? '' : 's'}` : ''}${ms > r.timeMs + 400 ? ` (${ms} ms round trip)` : ''}`, 'ok');
    }
  } catch (e) {
    if (e.name === 'AbortError' || seq !== state.compileSeq) return;
    setStatus(e.message, 'error');
  }
}

// ───────────────────────────── run (console pane) ─────────────────────────────

const SIGNAL_TEXT = {
  SIGSEGV: 'segmentation fault (invalid memory access)',
  SIGABRT: 'aborted (abort() or a failed assertion)',
  SIGFPE: 'arithmetic exception (for example division by zero)',
  SIGILL: 'illegal instruction',
  SIGBUS: 'bus error',
  SIGKILL: 'killed',
  SIGXCPU: 'CPU time limit exceeded',
  SIGXFSZ: 'file size limit exceeded',
  SIGPIPE: 'broken pipe',
  SIGSYS: 'blocked system call',
};

function span(cls, text) {
  const s = document.createElement('span');
  if (cls) s.className = cls;
  s.textContent = text;
  return s;
}

function renderRun(r) {
  const out = $('#console-output');
  out.textContent = '';
  if (!r.compiled) {
    out.append(span('out-stderr', 'Compilation failed, nothing was run.\n'));
    out.append(span('muted', 'See the Diagnostics tab.'));
    return;
  }
  if (r.stdout) out.append(span('', r.stdout));
  if (r.stderr) {
    if (r.stdout && !r.stdout.endsWith('\n')) out.append('\n');
    out.append(span('out-stderr', r.stderr));
  }
  if (!r.stdout && !r.stderr) out.append(span('muted', '(the program printed nothing)'));
  const meta = (cls, text) => { const m = span('out-meta ' + cls, text); out.append(m); };
  if (r.truncated) meta('bad', 'Output truncated: the program printed more than the limit allows and was stopped.');
  if (r.timedOut) meta('bad', 'Time limit exceeded: the program was stopped.');
  if (r.memoryExceeded) meta('bad', 'Memory limit exceeded: the program (with everything it started) was stopped.');
  if (r.signal) {
    meta('bad', `Terminated by ${r.signal}: ${SIGNAL_TEXT[r.signal] || 'signal'}`);
  } else if (r.exitCode !== null && r.exitCode !== undefined) {
    meta(r.exitCode === 0 ? 'good' : 'bad', `Exit code ${r.exitCode}`);
  }
}

async function runNow() {
  if (state.running || !state.runEnabled) return;
  state.running = true;
  const btn = $('#run');
  btn.disabled = true;
  btn.querySelector('span').textContent = 'Running…';
  setStatus('Compiling and running…');
  selectConsoleTab('output');
  if (document.body.dataset.view === 'source' && window.matchMedia('(max-width: 820px)').matches) setView('console');
  try {
    const r = await api('/api/run', { code: source.value, optLevel: state.opt, stdin: $('#stdin').value });
    renderRun(r);
    const { errors, warnings } = renderDiagnostics(r.diagnostics, r.compileStderr);
    if (!r.compiled) {
      selectConsoleTab('diag');
      setStatus(`${errors} error${errors === 1 ? '' : 's'}: nothing was run`, 'error');
    } else {
      setStatus(r.ok ? `Ran in ${r.timeMs} ms (compile ${r.compileMs} ms)` : 'The program did not exit normally', r.ok ? 'ok' : 'error');
    }
    $('#run-info').textContent = r.compiled ? `${r.timeMs} ms · ${warnings} warning${warnings === 1 ? '' : 's'}` : '';
  } catch (e) {
    const out = $('#console-output');
    out.textContent = '';
    out.append(span('out-stderr', e.message));
    setStatus(e.message, 'error');
  } finally {
    state.running = false;
    btn.disabled = false;
    btn.querySelector('span').textContent = 'Run';
  }
}

// ───────────────────────────── UI wiring ─────────────────────────────

function selectConsoleTab(tab) {
  for (const b of $$('#console-seg button')) b.setAttribute('aria-selected', String(b.dataset.tab === tab));
  $('#console-output').hidden = tab !== 'output';
  $('#console-diag').hidden = tab !== 'diag';
  $('#console-stdin').hidden = tab !== 'stdin';
}

function setView(view) {
  document.body.dataset.view = view;
  for (const b of $$('.tabbar button')) b.setAttribute('aria-pressed', String(b.dataset.view === view));
  // Monaco does not lay itself out while hidden
  window.dispatchEvent(new Event('resize'));
}

function setOpt(opt, recompile = true) {
  state.opt = opt;
  store.set('opt', String(opt));
  for (const b of $$('#opt-seg button')) b.setAttribute('aria-checked', String(Number(b.dataset.opt) === opt));
  if (recompile) compileNow();
}

function setEmit(emit, recompile = true) {
  state.emit = emit;
  store.set('emit', emit);
  for (const b of $$('#emit-seg button')) b.setAttribute('aria-selected', String(b.dataset.emit === emit));
  if (recompile) compileNow();
}

function setTheme(dark) {
  state.dark = dark;
  document.documentElement.dataset.theme = dark ? 'dark' : 'light';
  store.set('theme', dark ? 'dark' : 'light');
  if (usingMonaco && source) { source.setDark(dark); output.setDark(dark); }
}

function loadExample(ex) {
  source.value = ex.code;
  $('#stdin').value = ex.stdin || '';
  setOpt(ex.opt ?? 0, false);
  setEmit(ex.emit || 'asm', false);
  $('#console-output').innerHTML = '<span class="muted">Press Run to compile and execute the program.</span>';
  $('#run-info').textContent = '';
  if (ex.note) toast(ex.note);
  compileNow();
}

function fillExamples() {
  const sel = $('#examples');
  const groups = new Map();
  for (const ex of EXAMPLES) {
    if (!groups.has(ex.group)) groups.set(ex.group, []);
    groups.get(ex.group).push(ex);
  }
  for (const [name, list] of groups) {
    const og = document.createElement('optgroup');
    og.label = name;
    for (const ex of list) {
      const o = document.createElement('option');
      o.value = ex.id;
      o.textContent = ex.title;
      og.appendChild(o);
    }
    sel.appendChild(og);
  }
  sel.addEventListener('change', () => {
    const ex = EXAMPLES.find((e) => e.id === sel.value);
    if (ex) loadExample(ex);
    sel.value = '';
  });
}

// ───────────────────────────── sharing ─────────────────────────────

const toB64 = (bytes) => btoa(String.fromCharCode(...bytes)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
const fromB64 = (s) => Uint8Array.from(atob(s.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0));

async function pipe(bytes, stream) {
  const w = stream.writable.getWriter();
  w.write(bytes);
  w.close();
  return new Uint8Array(await new Response(stream.readable).arrayBuffer());
}

async function encodeState() {
  const json = new TextEncoder().encode(JSON.stringify({ c: source.value, o: state.opt, e: state.emit, i: $('#stdin').value }));
  if (typeof CompressionStream === 'function') return 's=' + toB64(await pipe(json, new CompressionStream('deflate-raw')));
  return 'j=' + toB64(json);
}

async function decodeState(hash) {
  const m = /^#?([sj])=([A-Za-z0-9_-]+)$/.exec(hash);
  if (!m) return null;
  try {
    let bytes = fromB64(m[2]);
    if (m[1] === 's') bytes = await pipe(bytes, new DecompressionStream('deflate-raw'));
    const v = JSON.parse(new TextDecoder().decode(bytes));
    if (typeof v.c !== 'string') return null;
    return { code: v.c, opt: [0, 1, 2].includes(v.o) ? v.o : 0, emit: OUTPUT_LANG[v.e] ? v.e : 'asm', stdin: typeof v.i === 'string' ? v.i : '' };
  } catch { return null; }
}

async function share() {
  const enc = await encodeState();
  const url = `${location.origin}${location.pathname}#${enc}`;
  history.replaceState(null, '', '#' + enc);
  try {
    await navigator.clipboard.writeText(url);
    toast(url.length > 8000 ? 'Link copied (it is long: the program is large)' : 'Link copied to the clipboard');
  } catch {
    toast('Link is in the address bar: copy it from there');
  }
}

// ───────────────────────────── splitters ─────────────────────────────

function setupSplitter(el, axis) {
  const root = document.documentElement;
  const key = axis === 'x' ? '--left' : '--top';
  const saved = store.get(key, '');
  if (saved) root.style.setProperty(key, saved);
  const apply = (pct) => {
    const v = Math.min(80, Math.max(20, pct));
    root.style.setProperty(key, v + '%');
    store.set(key, v + '%');
  };
  el.addEventListener('pointerdown', (e) => {
    e.preventDefault();
    el.setPointerCapture(e.pointerId);
    el.classList.add('dragging');
    const box = (axis === 'x' ? $('#panes') : $('.right')).getBoundingClientRect();
    const move = (ev) => apply(axis === 'x' ? ((ev.clientX - box.left) / box.width) * 100 : ((ev.clientY - box.top) / box.height) * 100);
    const up = () => {
      el.classList.remove('dragging');
      el.removeEventListener('pointermove', move);
      el.removeEventListener('pointerup', up);
      el.removeEventListener('pointercancel', up);
    };
    el.addEventListener('pointermove', move);
    el.addEventListener('pointerup', up);
    el.addEventListener('pointercancel', up);
  });
  el.addEventListener('keydown', (e) => {
    const cur = parseFloat(getComputedStyle(root).getPropertyValue(key)) || 50;
    const step = e.key === (axis === 'x' ? 'ArrowLeft' : 'ArrowUp') ? -3 : e.key === (axis === 'x' ? 'ArrowRight' : 'ArrowDown') ? 3 : 0;
    if (step) { e.preventDefault(); apply(cur + step); }
  });
}

// ───────────────────────────── start-up ─────────────────────────────

async function checkServer() {
  try {
    const r = await fetch('/api/health');
    const h = await r.json();
    $('#server-info').textContent = `cinder ${h.version} · sandbox: ${h.sandbox}`;
    state.runEnabled = !!h.runEnabled;
    if (!state.runEnabled) {
      const btn = $('#run');
      btn.disabled = true;
      btn.title = 'Running programs is disabled on this server';
      toast('Running programs is disabled on this server; compilation still works.');
    }
  } catch {
    $('#server-info').textContent = 'server not reachable';
  }
}

async function main() {
  setTheme(store.get('theme', matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light') === 'dark');
  await createEditors();
  setTheme(state.dark);
  fillExamples();
  setupSplitter($('#split-v'), 'x');
  setupSplitter($('#split-h'), 'y');

  // events
  for (const b of $$('#opt-seg button')) b.addEventListener('click', () => setOpt(Number(b.dataset.opt)));
  for (const b of $$('#emit-seg button')) b.addEventListener('click', () => setEmit(b.dataset.emit));
  for (const b of $$('#console-seg button')) b.addEventListener('click', () => selectConsoleTab(b.dataset.tab));
  for (const b of $$('.tabbar button')) b.addEventListener('click', () => setView(b.dataset.view));
  $('#run').addEventListener('click', runNow);
  $('#share').addEventListener('click', share);
  $('#theme').addEventListener('click', () => setTheme(!state.dark));
  $('#stdin').addEventListener('input', () => store.set('stdin', $('#stdin').value));
  source.onChange(() => { store.set('code', source.value); scheduleCompile(); });
  source.onRun(runNow);
  document.addEventListener('keydown', (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') { e.preventDefault(); runNow(); }
  });

  // linking source lines and output lines
  source.onCursorLine((line) => {
    const outs = state.srcToOut.get(line) || [];
    output.highlightLines(outs);
    if (outs.length) output.reveal(outs[0]);
  });
  output.onCursorLine((line) => {
    const src = state.lineMap[line - 1];
    source.highlightLines(src ? [src] : []);
    if (src) source.reveal(src);
  });

  // initial program: shared link > last session > hello world
  const shared = await decodeState(location.hash);
  if (shared) {
    source.value = shared.code;
    $('#stdin').value = shared.stdin;
    setOpt(shared.opt, false);
    setEmit(shared.emit, false);
  } else {
    source.value = store.get('code', '') || EXAMPLES[0].code;
    $('#stdin').value = store.get('stdin', '');
    setOpt(Number(store.get('opt', '0')) || 0, false);
    setEmit(store.get('emit', 'asm'), false);
  }
  checkServer();
  compileNow();
}

main().catch((e) => {
  console.error(e);
  setStatus('The playground failed to start: ' + e.message, 'error');
});
