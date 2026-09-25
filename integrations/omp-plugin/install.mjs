#!/usr/bin/env node
/**
 * Install Sakur4 into Oh My Pi.
 *
 * # Why this is a script rather than `omp install`
 *
 * `omp install <path>` creates a symlink from `~/.omp/plugins/node_modules/<name>`
 * to the source directory. On Windows that requires Developer Mode or an elevated
 * shell, and fails with a bare `EPERM: operation not permitted, symlink`. Since
 * Windows is where this project is developed and where OMP is most often run, the
 * documented install path does not work on the platform it is most needed.
 *
 * This script does what `omp install` would have done, without needing a symlink:
 *
 * 1. copies the plugin into `~/.omp/plugins/node_modules/omp-sakur4`,
 * 2. registers it in `~/.omp/plugins/package.json` dependencies,
 * 3. registers it in `~/.omp/plugins/omp-plugins.lock.json`,
 * 4. optionally installs the skill into `~/.agents/skills/`.
 *
 * Step 2 is the one that is easy to miss and impossible to diagnose. OMP's plugin
 * loader skips a lockfile entry that is neither declared in `package.json` nor a
 * symlink, logging only "skipping stale lockfile entry" to a log nobody reads —
 * so the plugin shows up in `omp plugin list` and `omp plugin doctor`, and never
 * loads. That failure is why this script writes both files.
 *
 * # Usage
 *
 *   node integrations/omp-plugin/install.mjs              # install everything
 *   node integrations/omp-plugin/install.mjs --no-skill   # plugin only
 *   node integrations/omp-plugin/install.mjs --uninstall
 */

import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir, platform } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const PLUGIN_NAME = "omp-sakur4";
const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "..", "..");

const args = process.argv.slice(2);
const flag = (name) => args.includes(`--${name}`);

if (flag("help") || flag("h")) {
  process.stdout.write(
    `Install Sakur4 into Oh My Pi.

  node integrations/omp-plugin/install.mjs [options]

Options:
  --no-skill     Install the plugin only, not the Agent Skill.
  --skill-only   Install the Agent Skill only, not the plugin.
  --dir <path>   Override the OMP home (default: ~/.omp).
  --agents <p>   Override the skills directory (default: ~/.agents/skills).
  --uninstall    Remove both.
  --help         This message.
`,
  );
  process.exit(0);
}

function argValue(name, fallback) {
  const index = args.indexOf(`--${name}`);
  if (index < 0) return fallback;
  const value = args[index + 1];
  if (!value || value.startsWith("--")) {
    fail(`--${name} needs a value`);
  }
  return value;
}

function fail(message) {
  process.stderr.write(`install: ${message}\n`);
  process.exit(1);
}

function info(message) {
  process.stdout.write(`${message}\n`);
}

const ompHome = resolve(argValue("dir", join(homedir(), ".omp")));
const agentsDir = resolve(argValue("agents", join(homedir(), ".agents", "skills")));
const pluginsRoot = join(ompHome, "plugins");
const pluginDir = join(pluginsRoot, "node_modules", PLUGIN_NAME);
const skillSource = join(repoRoot, "skills", "sakur4");
const skillTarget = join(agentsDir, "sakur4");

// ---------------------------------------------------------------------------
// Read and write the two registry files OMP consults
// ---------------------------------------------------------------------------

function readJson(path, fallback) {
  if (!existsSync(path)) return fallback;
  let raw;
  try {
    raw = readFileSync(path, "utf8");
  } catch (error) {
    fail(`could not read ${path}: ${error.message}`);
  }
  // Strip a UTF-8 BOM. Windows editors and several PowerShell cmdlets add one, and
  // `JSON.parse` rejects it — so a file that OMP itself reads happily would make
  // this installer refuse to run, with a message about a token nobody can see.
  if (raw.charCodeAt(0) === 0xfeff) raw = raw.slice(1);
  if (raw.trim() === "") return fallback;
  try {
    return JSON.parse(raw);
  } catch (error) {
    fail(`${path} is not valid JSON (${error.message}); fix or remove it and retry`);
  }
}

function writeJson(path, value) {
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`, "utf8");
}

// ---------------------------------------------------------------------------
// Uninstall
// ---------------------------------------------------------------------------

if (flag("uninstall")) {
  let removed = 0;
  if (existsSync(pluginDir)) {
    rmSync(pluginDir, { recursive: true, force: true });
    info(`removed ${pluginDir}`);
    removed++;
  }
  const pkgPath = join(pluginsRoot, "package.json");
  const pkg = readJson(pkgPath, null);
  if (pkg?.dependencies?.[PLUGIN_NAME]) {
    delete pkg.dependencies[PLUGIN_NAME];
    writeJson(pkgPath, pkg);
    info(`unregistered ${PLUGIN_NAME} from ${pkgPath}`);
  }
  const lockPath = join(pluginsRoot, "omp-plugins.lock.json");
  const lock = readJson(lockPath, null);
  if (lock?.plugins?.[PLUGIN_NAME]) {
    delete lock.plugins[PLUGIN_NAME];
    writeJson(lockPath, lock);
    info(`unregistered ${PLUGIN_NAME} from ${lockPath}`);
  }
  if (existsSync(skillTarget)) {
    rmSync(skillTarget, { recursive: true, force: true });
    info(`removed ${skillTarget}`);
    removed++;
  }
  info(removed ? "Uninstalled. Restart OMP." : "Nothing was installed.");
  process.exit(0);
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

if (!flag("skill-only")) {
  if (!existsSync(join(repoRoot, "integrations", "omp-plugin", "index.ts"))) {
    fail(`cannot find integrations/omp-plugin/index.ts under ${repoRoot}`);
  }

  mkdirSync(pluginDir, { recursive: true });
  cpSync(join(here, "index.ts"), join(pluginDir, "index.ts"));
  cpSync(join(here, "package.json"), join(pluginDir, "package.json"));

  // Bundle the skill with the plugin too, so the plugin's `resources_discover`
  // hook can offer it even when the portable location is not used.
  if (existsSync(skillSource)) {
    cpSync(skillSource, join(pluginDir, "skills", "sakur4"), { recursive: true });
  }

  info(`installed plugin  → ${pluginDir}`);

  const pkgPath = join(pluginsRoot, "package.json");
  const pkg = readJson(pkgPath, { name: "omp-plugins", private: true, dependencies: {} });
  pkg.dependencies ??= {};
  pkg.dependencies[PLUGIN_NAME] = "0.2.2";
  writeJson(pkgPath, pkg);
  info(`declared dependency in ${pkgPath}`);

  const lockPath = join(pluginsRoot, "omp-plugins.lock.json");
  const lock = readJson(lockPath, { plugins: {}, settings: {} });
  lock.plugins ??= {};
  lock.plugins[PLUGIN_NAME] = lock.plugins[PLUGIN_NAME] ?? {
    version: "0.2.2",
    enabledFeatures: null,
    enabled: true,
  };
  writeJson(lockPath, lock);
  info(`enabled in ${lockPath}`);
}

if (!flag("no-skill")) {
  if (!existsSync(join(skillSource, "SKILL.md"))) {
    fail(`cannot find ${join(skillSource, "SKILL.md")}`);
  }
  mkdirSync(agentsDir, { recursive: true });
  cpSync(skillSource, skillTarget, { recursive: true });
  info(`installed skill   → ${skillTarget}`);
  info("");
  info("The skill is in ~/.agents/skills, which is the Agent Skills standard");
  info("location. Claude Code, Codex, OMP, pi and anything else that reads that");
  info("directory will pick it up without further configuration.");
}

info("");
info("Next:");
info("  1. Put sakur4d on PATH (`cargo install sakur4d`) or set SAKUR4_BIN.");
info("  2. Restart OMP.");
info("  3. Ask it: \"list your sakur4 tools\" — there should be nine.");
