//! The local-files SDK: the Deno std filesystem helpers. The smallest an SDK
//! can be — an import pin and its documentation, no shim, no ops, no auth
//! (file access is governed by the sandbox's cwd permissions, not brokered).

use super::{Docs, Sdk};

/// Deno std filesystem helpers (walk, expandGlob, …), served as transpiled JS
/// via esm.sh's JSR proxy. Built on `Deno.*`, so it runs under the existing
/// cwd permissions with no extra wiring. See [`Sdk::imports`] on bumping pins.
const IMPORTS: &[(&str, &str)] = &[("@std/fs", "https://esm.sh/jsr/@std/fs@1.0.24")];

const SEED_DOC: &str = r#"// For local files, use the Deno std library and Deno globals (node:fs is NOT
// available). For example:
//   import { walk, expandGlob } from "@std/fs";
//   for await (const f of expandGlob("src/**/*.rs")) { /* f.path */ }   // find files
//   const text = await Deno.readTextFile("Cargo.toml");                 // read
//   await Deno.writeTextFile("path", text);                            // write / edit"#;

pub(crate) struct Files;

impl Sdk for Files {
    fn name(&self) -> &'static str {
        "files"
    }

    fn imports(&self) -> &'static [(&'static str, &'static str)] {
        IMPORTS
    }

    fn docs(&self) -> Docs {
        Docs {
            seed: SEED_DOC,
            system_prompt: None,
            import_blurb: "`@std/fs` (Deno file helpers)",
            mcp_blurb: "`@std/fs`",
        }
    }
}
