# mecha brand

The mark is 9A. An M whose outer strokes are two armour legs and whose vertex is
a notch cut into a heavy bar. A single slot sits between the legs; the legs
break at a knee and taper inward to two points on the ground. Same geometry at
every size and in the terminal.

## Geometry

Frame 63 × 54. Everything derives from one angle and one gap.

| Part | Value |
| --- | --- |
| Bar | y0–16, notch x24–39 cut 8.5 deep to a point at (31.5, 8.5) |
| Notch angle | 7.5 across per 8.5 down — the only slope in the mark |
| Gap | 4 units, used twice: bar to leg (y16–20) and at the knee (y35–39) |
| Upper leg | y20–35, x0–14 and x49–63 |
| Lower leg | y39–54, tapering inward at the notch angle to x27.24 / x35.76 |
| Slot | x21–42, y24–31 |

The feet stop 8.5 apart, which is the notch's own depth. The wedge of space
between them is a second V, pointing the same way as the one in the bar.

## Files

| File | Use |
| --- | --- |
| `logo.svg` | The mark. 63 × 54, accent-400 fill. Navbar, docs, anywhere ≥ 24px. |
| `logo-mono.svg` | Same paths, `fill="currentColor"` — inherits the surrounding text colour. |
| `logo-lockup.svg` | Mark + wordmark + descriptor. README, presentations, footers. |
| `favicon.svg` | 16px build. One deliberate deviation, below. |
| `apple-touch-icon.svg` | 180 × 180 on the void ground. |
| `og-card.svg` | 1200 × 630 social card. Rasterise to PNG before shipping. |
| `custom.css` | Docusaurus theme — copy to `website/src/css/custom.css`. |
| `splash.rs` | The block mark for the TUI, with the slot carrying run state. |
| `banner.md` | README header, image or code-fence version. |
| `contact-sheet.html` | Every file rendered at real sizes. Open it to check a change. |

### What the favicon changes, and why

A 16px favicon has 16 pixels to spend, so one thing in the full mark cannot
survive and is redrawn rather than left to the rasteriser: **the feet are
blunted.** The full taper reaches x27.24 of 63; the favicon stops at x28 of 64,
keeping a 2px gap between the tips.

Everything else is snapped to a 4-unit grid (= 1px at 16px), so no edge in the
favicon lands mid-pixel. The notch keeps its point, because the bar has material
below it.

Two things still need a raster step, which SVG can't do: `favicon.ico` (for old
browsers) and `og-card.png` (Twitter and Slack won't render SVG). Any
`svg → png` tool will do; the sources are here.

## Install

```
website/static/img/logo.svg          ← brand/logo.svg
website/static/img/favicon.svg       ← brand/favicon.svg
website/static/img/og-card.png       ← rasterised brand/og-card.svg
website/static/img/apple-touch-icon.png
website/src/css/custom.css           ← brand/custom.css
```

In `docusaurus.config.js`:

```js
favicon: 'img/favicon.svg',          // top level, not in themeConfig
themeConfig: {
  navbar: {
    title: 'mecha',
    logo: { alt: 'mecha', src: 'img/logo.svg' },
  },
  image: 'img/og-card.png',
  colorMode: { defaultMode: 'dark', respectPrefersColorScheme: false },
},
```

## Colour

| Token | Hex | Role |
| --- | --- | --- |
| void | `#12141f` | Page ground, footer, icon plates |
| bg | `#161826` | Panels, navbar |
| surface | `#232532` | Cards, armour fills |
| section | `#262a60` | The one saturated field: hero band |
| accent-400 | `#b5abfc` | The mark, links on dark, the lit slot |
| accent-500 | `#9184d9` | Base accent, focus ring |
| accent-700 | `#5d5294` | Panel lines, muted structure in the terminal |
| hazard | `#e8a24a` | Held sends, read-only, the one called-out rule |
| text | `#e9e9ed` | Body |
| text-muted | `#9a9aa8` | Secondary, labels |

Hazard amber never fills an area — lines, ticks and single characters only.
There is no second accent; contrast comes from the ramps, not from more hues.

## Type

Inter 500 for headings, Inter 400 for body, JetBrains Mono for code, labels,
kickers and anything a user would type. Never mono for prose.

The wordmark is lowercase `mecha` in Inter 500, tracking -0.02em, because that
is what you type. Uppercase wide-tracked MECHA is allowed only as a graphic in
the hero band and the README banner.

## Usage

Do

- Keep clear space equal to the bar's height (16 units) on all four sides.
- Below 24px use `favicon.svg`, not `logo.svg`.
- On a light ground swap the fill to accent-700 `#5d5294`.
- In the terminal, structure in accent-700 and the slot in accent-400, so the
  slot can turn hazard when a send is held.

Don't

- No gradient across the mark, no outer glow, no rotation, no outline.
- Don't fill an area with hazard amber.
- Don't set the wordmark in anything but Inter 500.
- Don't stretch the mark; the 7.5:8.5 angle is the proportion and it appears
  three times.
