/**
 * Obsidian CLI driver, for the parity suite only.
 *
 * Every hazard below was observed on a live Obsidian 1.13.7 and is the reason
 * this wrapper exists rather than a bare `Bun.spawn`:
 *
 *  - `vault=` must be the FIRST argument.
 *  - `base:query file="X"` needs the `.base` extension; `path=` is preferred.
 *  - Exit codes are meaningless: errors still exit 0. Output shape is the only
 *    trustworthy signal.
 *  - stdout is not flushed before exit; a heavy query can return an empty
 *    string, observed at 10 MB.
 *  - `base:views` ignores `file=`/`path=` and reads the active file.
 */

import { spawn } from "node:child_process";

export interface CliResult {
  stdout: string;
  stderr: string;
  code: number;
  /** True when the output looked truncated rather than genuinely empty. */
  truncated: boolean;
  /** True when the CLI printed an error, since the exit code will not say so. */
  errored: boolean;
}

export interface CliOptions {
  vault: string;
  /** Overrides the `obsidian` binary path. */
  binary?: string;
  /** Total attempts when output comes back empty and rows were expected. */
  maxAttempts?: number;
}

export class ObsidianCli {
  private readonly binary: string;

  constructor(private readonly options: CliOptions) {
    this.binary = options.binary ?? "obsidian";
  }

  /**
   * Whether the CLI can actually answer a query.
   *
   * Exit code is not the signal -- Obsidian exits 0 even when the bridge is
   * dead -- so this requires a non-empty version string. Without the length
   * check, a dead bridge reports AVAILABLE and every query then returns "",
   * which turns the whole parity suite into a vacuous green.
   */
  async available(): Promise<boolean> {
    try {
      const r = await this.run(["version"]);
      return !r.errored && r.code === 0 && r.stdout.trim() !== "";
    } catch {
      return false;
    }
  }

  /** A specific vault is open and answering, not just the app. */
  async vaultReachable(vault: string): Promise<boolean> {
    try {
      const r = await this.run([`vault=${vault}`, "bases"]);
      return !r.errored && r.stdout.trim() !== "";
    } catch {
      return false;
    }
  }

  async version(): Promise<string> {
    const r = await this.run(["version"]);
    return r.stdout.trim();
  }

  /** Run a command with `key=value` parameters. */
  async run(params: string[], extra: string[] = []): Promise<CliResult> {
    const attempts = this.options.maxAttempts ?? 3;
    let last: CliResult = {
      stdout: "",
      stderr: "",
      code: -1,
      truncated: false,
      errored: true,
    };

    for (let attempt = 0; attempt < attempts; attempt++) {
      last = await this.spawnOnce([`vault=${this.options.vault}`, ...params, ...extra]);
      // An empty stdout where we expect data is the stdout-flush race. Retry.
      if (last.stdout.trim() !== "" || !last.errored) return last;
    }
    return last;
  }

  private spawnOnce(args: string[]): Promise<CliResult> {
    return new Promise((resolve) => {
      const child = spawn(this.binary, args, { stdio: ["ignore", "pipe", "pipe"] });
      let stdout = "";
      let stderr = "";
      child.stdout.on("data", (d: Buffer) => {
        stdout += d.toString("utf8");
      });
      child.stderr.on("data", (d: Buffer) => {
        stderr += d.toString("utf8");
      });
      child.on("error", (err) => {
        resolve({
          stdout,
          stderr: `${stderr}${String(err)}`,
          code: -1,
          truncated: false,
          errored: true,
        });
      });
      child.on("close", (code) => {
        // Obsidian prints `Error: ...` and still exits 0, so detect it in text.
        const errored = /^\s*Error:/m.test(stdout) || /^\s*Error:/m.test(stderr);
        resolve({ stdout, stderr, code: code ?? -1, truncated: false, errored });
      });
    });
  }

  /**
   * Query a base. `path` is the exact vault-relative path including `.base`.
   */
  async queryBase(path: string, view?: string, format = "json"): Promise<CliResult> {
    const params = ["base:query", `path=${path}`, `format=${format}`];
    if (view !== undefined) params.push(`view=${view}`);
    return this.run(params);
  }

  /** Query and parse `format=json`. */
  async queryJson(path: string, view?: string): Promise<{ rows: unknown[]; raw: CliResult }> {
    const r = await this.queryBase(path, view, "json");
    const text = r.stdout.trim();
    if (text === "") return { rows: [], raw: r };
    try {
      const parsed = JSON.parse(text);
      return { rows: Array.isArray(parsed) ? parsed : [], raw: r };
    } catch {
      // A malformed body means we captured a partial write.
      return { rows: [], raw: { ...r, truncated: true } };
    }
  }

  /** Query `format=md`. */
  async queryMarkdown(path: string, view?: string): Promise<string> {
    const r = await this.queryBase(path, view, "md");
    return r.stdout.trim();
  }

  async listBases(): Promise<string[]> {
    const r = await this.run(["bases"]);
    if (r.errored) return [];
    return r.stdout
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => l.endsWith(".base"));
  }
}
