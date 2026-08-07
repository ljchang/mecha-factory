# brand

The marks, copied from [mecha's `brand/`](https://github.com/ljchang/mecha/tree/main/brand).

**mecha is the source of truth.** `brand/brand.md` there holds the geometry, the
colour tokens, the type rules and the usage rules; this directory is a copy of
the files this repository actually references, so a clone of the factory alone
still renders its README. If a mark changes, it changes there first.

| File | Use |
| --- | --- |
| `logo.svg` | The mark, accent-400. Dark grounds. |
| `logo-light.svg` | The mark, accent-700. Light grounds. |
| `logo-mono.svg` | `fill="currentColor"`, inherits the surrounding text colour. |
| `logo-lockup.svg` | Mark + wordmark + descriptor, for a dark ground. |
| `logo-lockup-light.svg` | The same on a light ground. |
| `favicon.svg` | The 16px build, with blunted feet. |

## What is already branded, and needs nothing from here

The **served surfaces** — the public form, the report shell — take their colour
from `mecha-manifest`'s `nocturne` theme, which is the brand palette expressed
as tokens (`mecha-manifest/src/theme.rs`). That is deliberate and is not a
duplicate: a theme is *tokens, never rules*, so the palette can be swapped
without touching layout, and `paper` exists beside it to prove the structural
sheet hardcodes no colour.

Two constraints there that this directory must not tempt anyone to break:

- **No `@import`, and no hosted font.** An imported stylesheet is an external
  reference: the publish gate fails a bundle for it, and the gate's own
  `style-src 'self'` blocks it outright. `nocturne` names Inter and JetBrains
  Mono with system fallback chains and downloads neither. Vendoring a `woff2`
  and serving it from our own origin is the supported route, and it is a
  separate decision from picking a palette.
- **A favicon is inlined as a `data:` URI**, for the same reason — a served page
  and a published bundle both have to be self-contained.
