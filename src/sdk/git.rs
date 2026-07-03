//! The local-git SDK: isomorphic-git (pure JS, no shell) plus the node-`fs`
//! adapter it needs. There is no auth facet here — pushes and fetches to
//! GitHub hit the git smart-HTTP endpoints, which the GitHub SDK claims and
//! authenticates (see `sdk::github`).

use super::{Docs, Sdk};

/// The core package and its fetch-based http client, pinned to the same
/// isomorphic-git version so they stay compatible; see [`Sdk::imports`] on
/// bumping pins.
const IMPORTS: &[(&str, &str)] = &[
    ("isomorphic-git", "https://esm.sh/isomorphic-git@1.38.6"),
    (
        "isomorphic-git/http/web",
        "https://esm.sh/isomorphic-git@1.38.6/http/web",
    ),
];

/// A minimal node-`fs`-shaped filesystem backed by `Deno.*`, exposed to guest
/// code as `globalThis.fs`. isomorphic-git takes an `fs` argument for every
/// working-tree/`.git` operation, but `node:fs` is not available in the sandbox;
/// this adapts the Deno APIs (which already run under the cwd read/write
/// permissions). Errors are remapped to node `errno` codes (ENOENT, EEXIST, …)
/// because isomorphic-git branches on `err.code` to detect missing refs/objects.
const FS_SHIM: &str = r#"
((globalThis) => {
    const mapErr = (e) => {
        if (e instanceof Deno.errors.NotFound) e.code = "ENOENT";
        else if (e instanceof Deno.errors.AlreadyExists) e.code = "EEXIST";
        else if (e instanceof Deno.errors.PermissionDenied) e.code = "EACCES";
        else if (e instanceof Deno.errors.NotADirectory) e.code = "ENOTDIR";
        else if (e instanceof Deno.errors.IsADirectory) e.code = "EISDIR";
        return e;
    };
    const toBytes = (data) =>
        typeof data === "string" ? new TextEncoder().encode(data)
        : data instanceof Uint8Array ? data
        : new Uint8Array(data);
    const toStats = (info) => ({
        isFile: () => info.isFile,
        isDirectory: () => info.isDirectory,
        isSymbolicLink: () => info.isSymlink,
        size: info.size,
        mode: info.mode ?? (info.isDirectory ? 0o40755 : 0o100644),
        ino: info.ino ?? 0,
        uid: info.uid ?? 0,
        gid: info.gid ?? 0,
        dev: info.dev ?? 0,
        mtimeMs: info.mtime?.getTime() ?? 0,
        ctimeMs: info.mtime?.getTime() ?? 0,
        mtime: info.mtime ?? new Date(0),
        ctime: info.mtime ?? new Date(0),
    });
    const promises = {
        readFile: async (path, opts) => {
            try {
                const bytes = await Deno.readFile(path);
                const enc = typeof opts === "string" ? opts : opts?.encoding;
                return enc ? new TextDecoder().decode(bytes) : bytes;
            } catch (e) { throw mapErr(e); }
        },
        writeFile: async (path, data) => {
            try { await Deno.writeFile(path, toBytes(data)); }
            catch (e) { throw mapErr(e); }
        },
        unlink: async (path) => {
            try { await Deno.remove(path); } catch (e) { throw mapErr(e); }
        },
        readdir: async (path) => {
            try {
                const names = [];
                for await (const entry of Deno.readDir(path)) names.push(entry.name);
                return names;
            } catch (e) { throw mapErr(e); }
        },
        mkdir: async (path, opts) => {
            try { await Deno.mkdir(path, { recursive: !!(opts && opts.recursive) }); }
            catch (e) { throw mapErr(e); }
        },
        rmdir: async (path) => {
            try { await Deno.remove(path); } catch (e) { throw mapErr(e); }
        },
        stat: async (path) => {
            try { return toStats(await Deno.stat(path)); } catch (e) { throw mapErr(e); }
        },
        lstat: async (path) => {
            try { return toStats(await Deno.lstat(path)); } catch (e) { throw mapErr(e); }
        },
        readlink: async (path) => {
            try { return await Deno.readLink(path); } catch (e) { throw mapErr(e); }
        },
        symlink: async (target, path) => {
            try { await Deno.symlink(target, path); } catch (e) { throw mapErr(e); }
        },
        chmod: async (path, mode) => {
            try { await Deno.chmod(path, mode); } catch (e) { throw mapErr(e); }
        },
    };
    globalThis.fs = { promises };
})(globalThis);
"#;

const SEED_DOC: &str = r#"// Local git runs via isomorphic-git, with a ready `fs` global wired to the
// working directory (there is NO shell — Deno.Command/child_process are
// blocked, so "git status" is `git.statusMatrix`, not a command). Import the
// bare specifier, never a URL. For example:
//   import git from "isomorphic-git";
//   const branch = await git.currentBranch({ fs, dir: "." });
//   const commits = await git.log({ fs, dir: ".", depth: 5 });           // recent history
//   const status = await git.statusMatrix({ fs, dir: "." });             // working-tree status
//   import http from "isomorphic-git/http/web";                          // remote repos
//   const url = "https://github.com/<owner>/<repo>.git";                 // https, not git@ SSH
//   const info = await git.getRemoteInfo({ http, url });                 // list refs
//   await git.push({ fs, http, dir: ".", url });                         // auth is brokered — do
//                                                                        // NOT pass onAuth"#;

pub(crate) struct Git;

impl Sdk for Git {
    fn name(&self) -> &'static str {
        "git"
    }

    fn imports(&self) -> &'static [(&'static str, &'static str)] {
        IMPORTS
    }

    fn docs(&self) -> Docs {
        Docs {
            seed: SEED_DOC,
            system_prompt: None,
            import_blurb: "`isomorphic-git` plus `isomorphic-git/http/web` (pure-JS git — a \
                 `fs` global wired to the working directory is provided, so \
                 `git.log`/`git.statusMatrix` work on `dir: \".\"`)",
            mcp_blurb: "`isomorphic-git` plus `isomorphic-git/http/web`",
        }
    }

    fn shim(&self) -> Option<&'static str> {
        Some(FS_SHIM)
    }
}
