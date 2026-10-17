// Builds nbs-native-host for the target Tauri is building and places it at
// src-tauri/binaries/nbs-native-host-<target-triple>[.exe], where
// `bundle.externalBin` (tauri.bundle.conf.json) expects it. Runs as the
// beforeBuildCommand of release builds; plain cargo builds don't need it.
import { execFileSync } from 'node:child_process';
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const hostTriple = execFileSync('rustc', ['-vV'], { encoding: 'utf8' }).match(/^host: (\S+)/m)[1];
const triple = process.env.TAURI_ENV_TARGET_TRIPLE || hostTriple;
const windows = triple.includes('windows');
const exe = windows ? '.exe' : '';

// Cross-compiling to Windows from Linux/macOS goes through cargo-xwin, the
// same runner the app itself is built with.
const crossWindows = windows && process.platform !== 'win32';
const args = [...(crossWindows ? ['xwin', 'build'] : ['build']), '--release', '-p', 'nbs-native-host', '--target', triple];
console.log(`> cargo ${args.join(' ')}`);
execFileSync('cargo', args, { cwd: root, stdio: 'inherit' });

const dest = join(root, 'src-tauri', 'binaries', `nbs-native-host-${triple}${exe}`);
mkdirSync(dirname(dest), { recursive: true });
copyFileSync(join(root, 'target', triple, 'release', `nbs-native-host${exe}`), dest);
console.log(`> native host ready: ${dest}`);
