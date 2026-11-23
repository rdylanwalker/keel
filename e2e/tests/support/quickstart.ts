/**
 * Parser for the README Quick Start, shared by the static check and the live run in
 * `09-readme-quickstart.spec.ts`.
 *
 * The README marks the section with `<!-- quickstart:begin ... -->` / `<!-- quickstart:end -->`.
 * Every fenced `bash` block in between must be preceded by `<!-- quickstart:step <id> <mode> -->`,
 * so a new command can never be added to the Quick Start without the live test running it.
 */
import fs from 'node:fs';
import path from 'node:path';

/** How the live spec treats a block (everything else runs exactly as written). */
export const QUICKSTART_MODES = [
  /** Run as written, in order, in the shared shell state. */
  'run',
  /** Run as written, but `curl` serves the release assets from a locally built tarball. */
  'install',
  /** Needs root: run as written when `sudo -n` works, otherwise verified / emulated. */
  'hosts',
  /** Long-running server: started in the background, stopped with SIGINT (Ctrl+C). */
  'serve',
  /** `keel auth login`: run as written, a test browser completes the opened login URL. */
  'login',
  /** A readiness check the README says to repeat: re-run until it exits 0. */
  'poll',
  /** The clean-up commands, run last. */
  'teardown',
  /** Optional extra (cosign): parsed and statically checked, not executed. */
  'optional',
] as const;

export type QuickstartMode = (typeof QUICKSTART_MODES)[number];

export interface QuickstartBlock {
  id: string;
  mode: QuickstartMode;
  /** The block body exactly as in the README (without the fences). */
  script: string;
  /** 1-based README line of the opening fence (for error messages). */
  line: number;
}

/** One `keel ...` invocation found in a block, as argv after `keel`. */
export interface KeelInvocation {
  blockId: string;
  argv: string[];
  source: string;
}

export const README_PATH = path.resolve(import.meta.dirname, '../../../README.md');

const BEGIN = /^<!--\s*quickstart:begin\b.*-->\s*$/;
const END = /^<!--\s*quickstart:end\s*-->\s*$/;
const STEP = /^<!--\s*quickstart:step\s+([a-z0-9-]+)\s+([a-z]+)\s*-->\s*$/;
const FENCE_OPEN = /^```(\w*)\s*$/;

export function readReadme(file = README_PATH): string {
  return fs.readFileSync(file, 'utf8');
}

/** Extracts the marked Quick Start blocks; throws on any structural problem. */
export function parseQuickstart(markdown: string): QuickstartBlock[] {
  const lines = markdown.split(/\r?\n/);
  const begins = lines.flatMap((l, i) => (BEGIN.test(l) ? [i] : []));
  const ends = lines.flatMap((l, i) => (END.test(l) ? [i] : []));
  if (begins.length !== 1 || ends.length !== 1 || ends[0] < begins[0]) {
    throw new Error(
      `README must contain exactly one <!-- quickstart:begin --> ... <!-- quickstart:end --> section (found ${begins.length} begin, ${ends.length} end)`,
    );
  }
  const blocks: QuickstartBlock[] = [];
  let pending: { id: string; mode: QuickstartMode; line: number } | null = null;
  for (let i = begins[0] + 1; i < ends[0]; i++) {
    const line = lines[i];
    const step = STEP.exec(line);
    if (step) {
      if (pending) throw new Error(`README:${pending.line}: quickstart:step has no code block`);
      const mode = step[2] as QuickstartMode;
      if (!QUICKSTART_MODES.includes(mode)) {
        throw new Error(`README:${i + 1}: unknown quickstart mode '${step[2]}'`);
      }
      pending = { id: step[1], mode, line: i + 1 };
      continue;
    }
    const fence = FENCE_OPEN.exec(line);
    if (!fence) {
      if (pending && line.trim() !== '') {
        throw new Error(`README:${pending.line}: quickstart:step must directly precede a block`);
      }
      continue;
    }
    if (!pending) {
      throw new Error(
        `README:${i + 1}: code block inside the Quick Start has no <!-- quickstart:step <id> <mode> --> marker`,
      );
    }
    if (fence[1] !== 'bash') {
      throw new Error(`README:${i + 1}: quickstart block '${pending.id}' must be a bash block`);
    }
    const body: string[] = [];
    let j = i + 1;
    for (; j < ends[0] && !/^```\s*$/.test(lines[j]); j++) body.push(lines[j]);
    if (j >= ends[0]) throw new Error(`README:${i + 1}: unterminated code block`);
    if (blocks.some((b) => b.id === pending!.id)) {
      throw new Error(`README:${pending.line}: duplicate quickstart step id '${pending.id}'`);
    }
    blocks.push({ id: pending.id, mode: pending.mode, script: body.join('\n'), line: i + 1 });
    pending = null;
    i = j;
  }
  if (pending) throw new Error(`README:${pending.line}: quickstart:step has no code block`);
  return blocks;
}

/**
 * Splits a block into shell words per simple command: joins `\` continuations, drops comments,
 * honours single/double quotes and splits on `;`, `&&`, `||` and `|`. Enough for README
 * commands; not a general shell parser.
 */
export function shellCommands(script: string): string[][] {
  const joined = script.replace(/\\\r?\n/g, ' ');
  const commands: string[][] = [];
  for (const rawLine of joined.split(/\r?\n/)) {
    let words: string[] = [];
    let word = '';
    let inWord = false;
    let quote: '"' | "'" | null = null;
    const flushWord = () => {
      if (inWord) words.push(word);
      word = '';
      inWord = false;
    };
    const flushCommand = () => {
      flushWord();
      if (words.length) commands.push(words);
      words = [];
    };
    for (let k = 0; k < rawLine.length; k++) {
      const c = rawLine[k];
      if (quote) {
        if (c === quote) quote = null;
        else word += c;
        continue;
      }
      if (c === '"' || c === "'") {
        quote = c;
        inWord = true;
      } else if (c === '#' && !inWord) {
        break;
      } else if (c === ' ' || c === '\t') {
        flushWord();
      } else if (c === ';' || c === '|' || (c === '&' && rawLine[k + 1] === '&')) {
        if (c !== ';' && rawLine[k + 1] === c) k++;
        flushCommand();
      } else {
        word += c;
        inWord = true;
      }
    }
    if (quote) throw new Error(`unterminated quote in: ${rawLine}`);
    flushCommand();
  }
  return commands;
}

/** Every `keel ...` command of the Quick Start (argv after `keel`). */
export function keelInvocations(blocks: QuickstartBlock[]): KeelInvocation[] {
  return blocks.flatMap((b) =>
    shellCommands(b.script)
      .filter((words) => words[0] === 'keel')
      .map((words) => ({ blockId: b.id, argv: words.slice(1), source: words.join(' ') })),
  );
}

/** The release version the install block pins (`.../releases/download/v<version>`). */
export function pinnedReleaseVersion(markdown: string): string {
  const pins = [...markdown.matchAll(/releases\/(?:download|tag)\/v(\d+\.\d+\.\d+)/g)].map(
    (m) => m[1],
  );
  const unique = [...new Set(pins)];
  if (unique.length !== 1) {
    throw new Error(`README must pin exactly one release version, found: ${unique.join(', ')}`);
  }
  const prose = /This README describes v(\d+\.\d+\.\d+)/.exec(markdown)?.[1];
  if (prose !== unique[0]) {
    throw new Error(`README says it describes v${prose} but pins v${unique[0]}`);
  }
  return unique[0];
}

/** Parses the hosts block: `echo '<ip> <name>...' | sudo tee -a /etc/hosts`. */
export function hostsEntries(script: string): { ip: string; names: string[] } {
  const m = /echo\s+'(\d+\.\d+\.\d+\.\d+)\s+([^']+)'/.exec(script);
  if (!m) throw new Error(`cannot parse hosts block: ${script}`);
  return { ip: m[1], names: m[2].trim().split(/\s+/) };
}

/** `--flag` names a help text documents (`-x, --flag <V>` / `--flag`). */
export function helpFlags(help: string): Set<string> {
  return new Set([...help.matchAll(/(?:^|[\s,])(--[a-z0-9][a-z0-9-]*)/gm)].map((m) => m[1]));
}

/** Subcommand names listed under `Commands:` in a clap help text. */
export function helpSubcommands(help: string): Set<string> {
  const out = new Set<string>();
  const lines = help.split(/\r?\n/);
  const start = lines.findIndex((l) => l.trim() === 'Commands:');
  if (start < 0) return out;
  for (const line of lines.slice(start + 1)) {
    if (line.trim() === '') break;
    const m = /^\s{2}([a-z0-9][a-z0-9-]*)\b/.exec(line);
    if (m) out.add(m[1]);
  }
  return out;
}

/**
 * The bootstrap summary prints the generated admin password. The live spec keeps the raw output in
 * memory (it parses the password from it), but the console (the CI log) and the kept `logs/*.log`
 * only get this redacted copy (NFR-SEC-05).
 */
export function redactSecrets(text: string): string {
  return text.replace(/(Admin Password:[ \t]*)(?!\[PROVIDED)\S+/g, '$1[REDACTED]');
}
