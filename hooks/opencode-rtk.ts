import type { Plugin } from "@opencode-ai/plugin"
import { execFile } from "node:child_process"
import { existsSync } from "node:fs"
import { homedir } from "node:os"
import { delimiter, join } from "node:path"
import { promisify } from "node:util"

// RTK OpenCode plugin — rewrites commands to use rtk for token savings.
// Requires: rtk >= 0.23.0.
//
// This is a thin delegating plugin: all rewrite logic lives in `rtk rewrite`,
// which is the single source of truth (src/discover/registry.rs).
// To add or change rewrite rules, edit the Rust registry — not this file.

const execFileAsync = promisify(execFile)

type BunShellResult = {
  stdout?: unknown
  exitCode?: number
  exited?: number
}

type BunShell = (
  strings: TemplateStringsArray,
  ...values: unknown[]
) => {
  quiet: () => {
    nothrow: () => Promise<BunShellResult>
  }
}

type ExecFileError = Error & {
  code?: number | string
  stdout?: unknown
}

let rtkPath: string | null | undefined

function resolveRtkPath(): string | null {
  if (rtkPath !== undefined) return rtkPath

  const candidates = [
    process.env.RTK_BIN,
    ...pathCandidates("rtk"),
    join(homedir(), ".local", "bin", "rtk"),
    join(homedir(), ".cargo", "bin", "rtk"),
    "/opt/homebrew/bin/rtk",
    "/usr/local/bin/rtk",
  ].filter(Boolean) as string[]

  rtkPath = candidates.find((candidate) => existsSync(candidate)) ?? null
  return rtkPath
}

function pathCandidates(binary: string): string[] {
  const dirs = (process.env.PATH ?? "").split(delimiter).filter(Boolean)
  const exts =
    process.platform === "win32"
      ? (process.env.PATHEXT ?? ".EXE;.CMD;.BAT;.COM").split(";")
      : [""]

  return dirs.flatMap((dir) => exts.map((ext) => join(dir, `${binary}${ext}`)))
}

function getBunShell(): BunShell | undefined {
  return (globalThis as typeof globalThis & { Bun?: { $?: BunShell } }).Bun?.$
}

function toText(value: unknown): string {
  if (value instanceof Uint8Array) return new TextDecoder().decode(value)
  return String(value ?? "")
}

function exitCodeFrom(value: BunShellResult): number {
  return value.exitCode ?? value.exited ?? 0
}

function rewrittenCommand(
  command: string,
  stdout: unknown,
  exitCode: number,
): string | null {
  if (exitCode !== 0) return null

  const rewritten = toText(stdout).trim()
  return rewritten && rewritten !== command ? rewritten : null
}

async function rewriteWithBun(
  rtk: string,
  command: string,
): Promise<string | null | undefined> {
  const shell = getBunShell()
  if (!shell) return undefined

  try {
    const result = await shell`${rtk} rewrite ${command}`.quiet().nothrow()
    return rewrittenCommand(command, result.stdout, exitCodeFrom(result))
  } catch {
    return undefined
  }
}

async function rewriteWithNode(
  rtk: string,
  command: string,
): Promise<string | null> {
  try {
    const result = await execFileAsync(rtk, ["rewrite", command], {
      encoding: "utf8",
      timeout: 2000,
    })
    return rewrittenCommand(command, result.stdout, 0)
  } catch (error) {
    const childError = error as ExecFileError
    const exitCode =
      typeof childError.code === "number" ? childError.code : 1

    return rewrittenCommand(command, childError.stdout, exitCode)
  }
}

async function tryRewrite(command: string): Promise<string | null> {
  const rtk = resolveRtkPath()
  if (!rtk) return null

  const bunResult = await rewriteWithBun(rtk, command)
  return bunResult === undefined ? rewriteWithNode(rtk, command) : bunResult
}

export const RtkOpenCodePlugin: Plugin = async () => {
  if (!resolveRtkPath()) {
    console.warn("[rtk] rtk binary not found — plugin disabled")
    return {}
  }

  return {
    "tool.execute.before": async (input, output) => {
      const tool = String(input?.tool ?? "").toLowerCase()
      if (tool !== "bash" && tool !== "shell") return
      const args = output?.args
      if (!args || typeof args !== "object") return

      const command = (args as Record<string, unknown>).command
      if (typeof command !== "string" || !command) return

      try {
        const rewritten = await tryRewrite(command)
        if (rewritten) {
          ;(args as Record<string, unknown>).command = rewritten
        }
      } catch {
        // rtk rewrite failed — pass through unchanged
      }
    },
  }
}
