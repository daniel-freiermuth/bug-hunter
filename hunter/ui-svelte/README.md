# hunter UI

Svelte 5 frontend for the hunter daemon. Replaces the hand-rolled
TypeScript app that previously lived in `../ui/src/app.ts`.

## Layout

| Path | Contents |
| --- | --- |
| `src/pages/` | One component per nav page: Status, Inbox, Kanban, All Findings, Repos, Stats, Log |
| `src/components/` | Shared widgets: `FilterBar`, `FindingCard`, `FindingDetail`, `MultiSelect` |
| `src/lib/api.svelte.ts` | Polling store and the `post()` helper (sets `Content-Type`, which the server's POST gate requires) |
| `src/lib/types.ts` | Hand-maintained mirror of the server's JSON shapes — see `hunter-rs/API-CONTRACT.md` |
| `src/lib/format.ts` | Timestamp, duration and token formatting |

State uses runes (`$state`, `$derived`, `$effect`). Reactive collections
are `SvelteMap`/`SvelteSet` and must be **mutated in place** — replacing
the instance breaks fine-grained invalidation.

## Develop

```sh
npm install
npm run dev
```

The dev server proxies `/api` to the Rust daemon on `127.0.0.1:8377`
(`vite.config.ts`), so start the backend alongside it:

```sh
cd ../../hunter-rs && cargo run -- --root ../hunter
```

The binary has a single mode: it takes the lockfile, runs migrations and
runs the scheduler loop, so only one instance can own a given `--root`.

## Validate

```sh
npm run check
```

Runs `svelte-check` against `tsconfig.app.json` plus `tsc` on the Vite
config. CI runs this, ESLint and the production build.

> A bare `npx svelte-check` checks almost nothing here: the root
> `tsconfig.json` is a project-references stub with `files: []`, so none
> of the app's compiler options apply. Always go through `npm run check`
> or pass `--tsconfig ./tsconfig.app.json` explicitly.

## Build

```sh
npm run build
```

Vite writes to `../ui/` (`emptyOutDir: true`), which the Rust binary
serves as static files. **The build output is committed**, because the
daemon ships as a single binary beside a directory of static files with
no node toolchain on the target host — so rebuild and commit `../ui/`
whenever you change anything here.
