use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub markup: Markup,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let default_path;
        let path = if let Some(path) = path {
            path
        } else {
            let Some(path) = config_path() else {
                return Ok(Self::default());
            };
            let parent = path.parent().expect("default config has a parent");
            fs::create_dir_all(parent).with_context(|| {
                format!("cannot create configuration directory {}", parent.display())
            })?;
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => file
                    .write_all(include_bytes!("../config.default.toml"))
                    .with_context(|| format!("cannot write configuration {}", path.display()))?,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("cannot create configuration {}", path.display())
                    });
                }
            }
            default_path = path;
            &default_path
        };
        let source = fs::read_to_string(path)
            .with_context(|| format!("cannot read configuration {}", path.display()))?;
        toml::from_str(&source).with_context(|| format!("invalid configuration {}", path.display()))
    }
}

fn config_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .map(|root| root.join("mdterm/config.toml"))
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Markup {
    enabled: bool,
    dark: Palette,
    light: Palette,
}

impl Default for Markup {
    fn default() -> Self {
        Self {
            enabled: true,
            dark: Palette::default(),
            light: Palette::default(),
        }
    }
}

impl Markup {
    pub fn colors(&self, dark: bool) -> impl Iterator<Item = (&'static str, Color)> {
        let palette = if dark { &self.dark } else { &self.light };
        let [code, strong, emphasis, heading, link, quote, strike] = if dark {
            [
                0xc3e88d, 0xff966c, 0xc099ff, 0x82aaff, 0x86e1fc, 0x828bb8, 0x828bb8,
            ]
        } else {
            [
                0x0a3069, 0xcf222e, 0x8250df, 0x0550ae, 0x0969da, 0x6e7781, 0x6e7781,
            ]
        };
        let enabled = self.enabled;
        [
            ("inline-code", palette.inline_code, code),
            ("strong", palette.strong, strong),
            ("emphasis", palette.emphasis, emphasis),
            ("heading", palette.heading, heading),
            ("link", palette.link, link),
            ("quote", palette.quote, quote),
            ("strike", palette.strike, strike),
        ]
        .into_iter()
        .map(move |(name, color, fallback)| {
            (
                name,
                if enabled {
                    color.unwrap_or(Color::Rgb(fallback))
                } else {
                    Color::Inherit
                },
            )
        })
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Palette {
    inline_code: Option<Color>,
    strong: Option<Color>,
    emphasis: Option<Color>,
    heading: Option<Color>,
    link: Option<Color>,
    quote: Option<Color>,
    strike: Option<Color>,
}

#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(try_from = "String")]
pub enum Color {
    Inherit,
    Rgb(u32),
}

impl TryFrom<String> for Color {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value == "inherit" {
            return Ok(Self::Inherit);
        }
        value
            .strip_prefix('#')
            .filter(|hex| hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .map(Self::Rgb)
            .ok_or("expected #RRGGBB or inherit")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn partial_palettes_inherit_the_selected_theme_and_can_disable_colors() {
        let config: Config =
            toml::from_str("[markup.light]\nstrong = '#12aBcD'\ninline_code = 'inherit'\n")
                .unwrap();
        let defaults = Config::default();
        let mut expected = defaults.markup.colors(false).collect::<BTreeMap<_, _>>();
        expected.insert("strong", Color::Rgb(0x12abcd));
        expected.insert("inline-code", Color::Inherit);
        assert_eq!(
            config.markup.colors(false).collect::<BTreeMap<_, _>>(),
            expected
        );
        assert_eq!(
            config.markup.colors(true).collect::<BTreeMap<_, _>>(),
            defaults.markup.colors(true).collect::<BTreeMap<_, _>>()
        );

        let disabled: Config =
            toml::from_str("[markup]\nenabled = false\n[markup.dark]\nstrong = '#123456'\n")
                .unwrap();
        for dark in [false, true] {
            assert!(
                disabled
                    .markup
                    .colors(dark)
                    .all(|(_, color)| color == Color::Inherit)
            );
        }
    }

    #[test]
    fn invalid_colors_and_unknown_settings_are_rejected() {
        for color in [
            "red",
            "#123",
            "#12345678",
            "#gg1234",
            "#+12345",
            "inherit\";panic()",
        ] {
            let source = format!("[markup.dark]\nstrong = '{}'\n", color);
            assert!(
                toml::from_str::<Config>(&source).is_err(),
                "accepted {color}"
            );
        }
        for source in [
            "[markup]\nenabeld = false\n",
            "[markup.light]\nstrnog = '#123456'\n",
            "[other]\nenabled = true\n",
        ] {
            assert!(
                toml::from_str::<Config>(source).is_err(),
                "accepted {source}"
            );
        }
    }
}
