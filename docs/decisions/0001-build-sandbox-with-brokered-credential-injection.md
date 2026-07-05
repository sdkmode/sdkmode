---
status: "accepted"
date: 2026-06-12
decision-makers: Heidi Hill <heidi@heidi.codes>
consulted:
informed:
---

# Build the sandbox with brokered credential injection

## Context and Problem Statement

sdkmode runs JavaScript written by a language model against real, third-party SDKs — the GitHub API, and others over time — so the model can get useful work done (list issues, open a PR, read a repo) rather than just describe how.
That code is untrusted: the model can be prompted into writing anything, and the SDKs it calls need real credentials to authenticate.

The problem is how to let model-written code make authenticated API calls without ever letting the model — or the code it writes — see the credentials.
A token that reaches the guest can be exfiltrated (printed, POSTed to an attacker, smuggled out in a returned value); once it leaks it authorises actions well beyond the task at hand.
We also don't trust the guest not to try reading the host filesystem, spawning processes, or reaching internal network services.

## Decision Drivers

* Confidentiality of credentials — the model and its generated code must never observe an API token or the host environment. This is the primary constraint.
* Least privilege — the guest should hold only the capabilities a given task needs, so a hostile or buggy snippet has little to reach for.
* Real authenticated SDKs — the whole point is that the agent uses genuine, first-party SDKs (Octokit, etc.) against live APIs, so the design can't rely on stubs or a curated proxy API.
* SSRF protection — arbitrary outbound network from the guest must not become a pivot into loopback, private, or cloud-metadata addresses.
* Ownership cost — however we broker auth, we have to build and maintain it ourselves per SDK.

## Considered Options

* Give the model the tokens directly (inject them into the guest environment or hand them to the code)
* Run the code in-process, unsandboxed, and trust it
* Sandbox the guest at least privilege and inject credentials host-side, as requests leave the sandbox (a fetch broker)

## Decision Outcome

Chosen option: **sandbox plus host-side fetch broker**, because it is the only option that satisfies the confidentiality constraint while still letting the agent drive real authenticated SDKs.

Model-written JavaScript runs in a locked-down Deno runtime with least-privilege permissions: read and write are scoped to the current working directory only, and there is no environment access, no subprocess, and no FFI (`src/sandbox.rs`, `PermissionsOptions { allow_read: [cwd], allow_write: [cwd], allow_net: [] , .. }`).
Network is *allowed* at the Deno layer but every outbound request is funnelled through a host-side broker before it leaves the process.
The broker is a `request_builder_hook` installed on the vendored `deno_fetch` (`src/fetch.rs`, `RequestInterceptor`): as each request departs, the host matches its target against the registry of known SDK API hosts and, only then, injects that SDK's auth header (and cookies) — authenticating on the guest's behalf without the token ever entering the guest.
Requests to a host that isn't a registered SDK carry no credentials, and requests to loopback / private / link-local / cloud-metadata targets are refused outright (`is_blocked_host`).

The credential therefore lives entirely host-side: the guest emits an ordinary `fetch`/SDK call, and authentication is a thing that happens *to* the request after it leaves the sandbox, invisibly.

### Consequences

* Good, because credentials are structurally out of the guest's reach — there is no token in the environment, in a variable, or in any API surface the model can read, so it cannot leak what it never holds.
* Good, because the agent still uses real, first-party SDKs against live endpoints; brokering is transparent, so nothing about the SDK's own code has to change.
* Good, because least privilege limits the blast radius of a hostile snippet: no env, no subprocess, no FFI, and filesystem access confined to the working directory.
* Good, because only registered SDK hosts ever receive auth, so a request the model aims at an arbitrary host cannot ride out on someone else's credentials.
* Good, because the SSRF blocklist keeps arbitrary outbound network from becoming a pivot into internal or metadata addresses.
* Bad, because the broker must be maintained per SDK — each new SDK needs host code that knows its API host and how to authenticate it (bearer vs. the git smart-HTTP Basic special-case in `src/fetch.rs` is a taste of this).
* Bad, because loading npm packages on demand from esm.sh makes esm.sh a trusted third party in the supply chain; a compromised or unavailable esm.sh degrades or endangers the guest.
* Bad, because the SSRF blocklist is best-effort. It inspects the request host but does not resolve names, so a public hostname that resolves to a private address (DNS rebinding) is not caught here — that would need a connect-time check in the HTTP connector.

### Confirmation

The permission set is asserted where it's built in `src/sandbox.rs`, and the broker's host-matching and SSRF blocking are covered by the unit tests in `src/fetch.rs` (`blocks_named_ssrf_targets`, `blocks_plain_v4_ranges`, `blocks_ipv4_mapped_*`, `blocks_decimal_literal_loopback`, and the corresponding "allows public" cases).
A code review of any new SDK should confirm it is added to the registry with a correct API host and auth scheme, so that brokering — and only brokering — grants it credentials.

## Pros and Cons of the Options

### Give the model the tokens directly

Inject credentials into the guest (its environment, or as an argument to the code) and let SDKs authenticate normally inside the sandbox.

* Good, because it is trivial — SDKs just work, no broker to build.
* Good, because it needs no per-SDK host code.
* Bad, because it violates the primary constraint outright: the model and its code can read the token and exfiltrate it. A single `console.log`/`fetch` leaks it.
* Bad, because a leaked token authorises far more than the task, with no containment.

### Run the code in-process, unsandboxed

Execute the model's JavaScript directly with full host privileges and ambient credentials.

* Good, because it is the simplest possible thing and every SDK and npm package "just works".
* Bad, because it grants untrusted code the entire host: filesystem, environment, subprocess, network — the opposite of least privilege.
* Bad, because credentials in the host environment are directly readable by the guest.

### Sandbox plus host-side fetch broker (chosen)

Least-privilege Deno sandbox for the guest; credentials injected host-side by a fetch broker as requests leave.

* Good, because the guest never holds the credential, satisfying the confidentiality driver by construction.
* Good, because it still runs real SDKs against live APIs — brokering is transparent to the SDK code.
* Good, because least privilege plus an SSRF blocklist contain a hostile snippet.
* Neutral, because it moves complexity into the host, where we can review and test it.
* Bad, because we own and maintain the broker per SDK, and depend on esm.sh for package loading.
* Bad, because the SSRF defence is best-effort (no DNS-rebinding protection at this layer).

## More Information

* The broker lives in `src/fetch.rs` (`RequestInterceptor` / `is_blocked_host`); the permission set is in `src/sandbox.rs`.
* Brokering depends on a fork of `deno_fetch` that adds the `request_builder_hook` used to rewrite outgoing requests — see `vendor/deno_fetch/PATCHES.md`.
* [ADR-0002](0002-drive-the-llm-in-a-repl-instead-of-exposing-mcp.md) builds on this decision; the credential broker is unchanged by the REPL-vs-MCP choice and applies identically to both interfaces.
* **Revisit trigger:** the SSRF blocklist is best-effort and does not defend against DNS rebinding. If the guest is ever exposed to a stronger threat model (e.g. multi-tenant, or untrusted network targets by design), add a connect-time address check in the HTTP connector before relying on this layer alone.
