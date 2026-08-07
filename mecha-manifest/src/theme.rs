//! Themes: the palette a form is rendered in, as data.
//!
//! A schema-driven form should look designed without anyone designing it, and
//! certainly without an *agent* designing it — a model that writes CSS per
//! request produces a different form every time, which is the opposite of a
//! surface people learn to trust. So the layout is fixed and shared, and a
//! theme supplies colours, radii and type. Nothing else.
//!
//! **A theme is tokens, never rules.** [`Theme::css`] emits custom properties
//! and stops. If a theme could add selectors it would immediately become the
//! place layout fixes go — "just this one form needs the label wider" — and
//! within a month no two forms would render alike. The structural sheet
//! references `var(--…)` exclusively and hardcodes no colour at all, which is
//! what makes swapping a palette a swap rather than a rewrite.
//!
//! **Both schemes, always.** A theme declares light and dark, and the sheet
//! picks with `prefers-color-scheme`. `[data-theme="dark"]` and
//! `[data-theme="light"]` override it, so a toggle can be added later without
//! touching a theme. A theme that shipped one scheme would force a reader onto
//! whichever the author happened to use.
//!
//! **Type is a stack, not a download.** A theme names families with a system
//! fallback chain, and nothing here fetches a font: an `@import` of a hosted
//! stylesheet is an external reference, which the publish gate fails a bundle
//! for and the gate's own `style-src 'self'` blocks outright. Vendoring a
//! `woff2` and serving it from our origin is the supported route, and it is a
//! separate decision from picking a palette.

/// One palette. Every field is a CSS colour, and the names are roles rather
/// than hues — a theme that called a token `purple` would be a theme nobody
/// could recolour.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// The page behind everything.
    pub ground: &'static str,
    /// Cards, inputs, anything raised off the ground.
    pub surface: &'static str,
    /// Body text.
    pub text: &'static str,
    /// Labels, help text, anything secondary.
    pub muted: &'static str,
    /// Hairlines: input borders, rules, the edge of a fieldset.
    pub line: &'static str,
    /// The one saturated colour. Buttons, links, the checked state.
    pub accent: &'static str,
    /// Text *on* the accent — computed by the theme's author, not derived,
    /// because contrast is a judgement about a specific pair.
    pub on_accent: &'static str,
    /// The focus ring. Distinct from `accent` on purpose: a ring has to be
    /// visible against the accent itself, since a focused button has both.
    pub ring: &'static str,
    /// Something called out: a failed field, a held send. Used on rules,
    /// ticks and single characters — never as a fill.
    pub signal: &'static str,
}

/// A named look.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    /// One line for whoever is choosing.
    pub description: &'static str,
    pub light: Palette,
    pub dark: Palette,
    /// Body and heading type.
    pub sans: &'static str,
    /// Labels, and anything the reader would type. Its use on *labels* is what
    /// gives a form its character: a field label is a name for a value, which
    /// is closer to code than to prose.
    pub mono: &'static str,
    /// Corner radius on inputs and buttons, as a CSS length.
    pub radius: &'static str,
}

/// `nocturne` — the mecha palette. Dark-first; the light scheme is a courtesy.
///
/// Taken from the brand: the accent ramp, the grounds, and hazard amber as the
/// signal. Hazard is deliberately mapped to `signal` rather than to a second
/// accent, because the brand has no red and says so — "there is no second
/// accent; contrast comes from the ramps, not from more hues".
pub const NOCTURNE: Theme = Theme {
    name: "nocturne",
    description: "The mecha palette. Dark-first, violet accent, amber for anything called out.",
    dark: Palette {
        ground: "#12141f",  // void
        surface: "#161826", // bg
        text: "#e9e9ed",
        muted: "#9a9aa8",
        line: "#2b2741", // accent-900, so structure reads as the same family
        accent: "#b5abfc",
        on_accent: "#12141f",
        ring: "#9184d9", // accent-500, named as the focus ring in the brand
        signal: "#e8a24a",
    },
    light: Palette {
        ground: "#ffffff",
        surface: "#f7f7fa",
        text: "#1a1a22",
        muted: "#5a5a68",
        line: "#dcdce4",
        // On a light ground the brand swaps the mark to accent-700, and the
        // accent has to carry white text, which accent-400 cannot.
        accent: "#5d5294",
        on_accent: "#ffffff",
        ring: "#9184d9",
        signal: "#96601f",
    },
    sans: "Inter, system-ui, -apple-system, \"Segoe UI\", sans-serif",
    mono: "\"JetBrains Mono\", ui-monospace, SFMono-Regular, Menlo, monospace",
    radius: "8px",
};

/// `paper` — a neutral, ink-on-white alternative.
///
/// Here because **one built-in theme is a stylesheet with extra steps.** A
/// second, deliberately unlike the first, is what proves the structural sheet
/// hardcodes nothing: if switching to this leaks a violet edge or a dark
/// ground anywhere, a colour is written somewhere it should not be.
pub const PAPER: Theme = Theme {
    name: "paper",
    description: "Ink on paper. Light-first, restrained, no house colour.",
    light: Palette {
        ground: "#fdfdfc",
        surface: "#ffffff",
        text: "#1c1b1a",
        muted: "#6b6764",
        line: "#e0ddd8",
        accent: "#1c1b1a",
        on_accent: "#fdfdfc",
        ring: "#8a857f",
        signal: "#9a5b12",
    },
    dark: Palette {
        ground: "#191817",
        surface: "#211f1e",
        text: "#eceae7",
        muted: "#a09b95",
        line: "#38352f",
        accent: "#eceae7",
        on_accent: "#191817",
        ring: "#a09b95",
        signal: "#d99a4e",
    },
    sans: "system-ui, -apple-system, \"Segoe UI\", sans-serif",
    mono: "ui-monospace, SFMono-Regular, Menlo, monospace",
    radius: "4px",
};

pub const BUILT_IN: [Theme; 2] = [NOCTURNE, PAPER];

/// The house palette. Named here rather than left to each caller's own
/// `unwrap_or`, so "the default theme" is one fact with one place to change it.
impl Default for Theme {
    fn default() -> Self {
        NOCTURNE
    }
}

impl Theme {
    /// Look one up by name. Unknown names fall back to the default rather than
    /// failing: a typo in a deployment's config must not take the forms down,
    /// and the wrong palette is a visible problem that fixes itself.
    pub fn by_name(name: &str) -> Theme {
        BUILT_IN
            .into_iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
            .unwrap_or(NOCTURNE)
    }

    /// The custom properties, and nothing else.
    ///
    /// Three blocks: the light scheme as the base, the dark scheme under
    /// `prefers-color-scheme`, and both again under `[data-theme]` so an
    /// explicit choice beats the system one in either direction.
    pub fn css(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "/* theme: {} — {} */\n:root {{\n  color-scheme: light dark;\n\
             \x20 --font-sans: {};\n  --font-mono: {};\n  --radius: {};\n{}}}\n",
            self.name,
            self.description,
            self.sans,
            self.mono,
            self.radius,
            vars(&self.light)
        ));
        out.push_str(&format!(
            "@media (prefers-color-scheme: dark) {{\n  :root {{\n{}  }}\n}}\n",
            indent(&vars(&self.dark))
        ));
        out.push_str(&format!(
            ":root[data-theme=\"light\"] {{\n{}}}\n:root[data-theme=\"dark\"] {{\n{}}}\n",
            vars(&self.light),
            vars(&self.dark)
        ));
        out
    }
}

fn vars(p: &Palette) -> String {
    [
        ("ground", p.ground),
        ("surface", p.surface),
        ("text", p.text),
        ("muted", p.muted),
        ("line", p.line),
        ("accent", p.accent),
        ("on-accent", p.on_accent),
        ("ring", p.ring),
        ("signal", p.signal),
    ]
    .iter()
    .map(|(name, value)| format!("  --{name}: {value};\n"))
    .collect()
}

fn indent(block: &str) -> String {
    block
        .lines()
        .map(|l| format!("  {l}\n"))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant the whole module exists for: a theme contributes
    /// declarations and never selectors. The moment one can add a rule, it
    /// becomes where per-form layout hacks live.
    #[test]
    fn a_theme_emits_tokens_and_never_rules() {
        for theme in BUILT_IN {
            let css = theme.css();
            // The only selectors permitted are the three that scope the tokens.
            for line in css.lines() {
                let line = line.trim();
                if line.ends_with('{') {
                    assert!(
                        line.starts_with(":root") || line.starts_with("@media") || line == "{",
                        "theme `{}` opened a rule that is not a token scope: {line}",
                        theme.name
                    );
                }
            }
        }
    }

    /// Every theme ships both schemes. One that did not would force whoever
    /// reads the form onto whichever the author happened to be using.
    #[test]
    fn every_theme_defines_every_token_in_both_schemes() {
        for theme in BUILT_IN {
            let css = theme.css();
            for token in [
                "--ground",
                "--surface",
                "--text",
                "--muted",
                "--line",
                "--accent",
                "--on-accent",
                "--ring",
                "--signal",
            ] {
                // Once in :root, once under prefers-color-scheme, and once in
                // each explicit [data-theme] block.
                assert!(
                    css.matches(&format!("{token}:")).count() >= 4,
                    "theme `{}` is missing `{token}` in a scheme",
                    theme.name
                );
            }
            assert!(css.contains("prefers-color-scheme: dark"), "{}", theme.name);
            assert!(css.contains("[data-theme=\"light\"]"), "{}", theme.name);
        }
    }

    /// An unknown name is a typo in a config file, not a reason to stop
    /// serving forms.
    #[test]
    fn an_unknown_theme_falls_back_rather_than_failing() {
        assert_eq!(Theme::by_name("nocturne").name, "nocturne");
        assert_eq!(Theme::by_name("NOCTURNE").name, "nocturne");
        assert_eq!(Theme::by_name("paper").name, "paper");
        assert_eq!(Theme::by_name("mauve-deluxe").name, NOCTURNE.name);
    }

    /// A palette must not name a hue. `--purple` would be a token nobody could
    /// re-theme, and the second theme is what would break on it.
    #[test]
    fn tokens_are_named_for_roles_rather_than_colours() {
        let css = NOCTURNE.css();
        for hue in ["purple", "violet", "amber", "blue", "green", "red"] {
            assert!(
                !css.contains(&format!("--{hue}")),
                "a token is named after a hue: {hue}"
            );
        }
    }
}
