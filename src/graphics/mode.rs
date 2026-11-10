// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum GraphicsMode {
    Integrated,
    Compute,
    Hybrid,
    Discrete,
}

impl From<GraphicsMode> for &'static str {
    fn from(mode: GraphicsMode) -> &'static str {
        match mode {
            GraphicsMode::Integrated => "integrated",
            GraphicsMode::Compute => "compute",
            GraphicsMode::Hybrid => "hybrid",
            GraphicsMode::Discrete => "nvidia",
        }
    }
}

/// Error returned when a string does not name a graphics mode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid graphics mode '{0}' (expected one of: integrated, hybrid, compute, nvidia)")]
pub struct InvalidGraphicsMode(pub String);

impl std::str::FromStr for GraphicsMode {
    type Err = InvalidGraphicsMode;

    /// Parse a mode name as used on the CLI and over D-Bus.
    ///
    /// Unknown names are rejected rather than mapped to a default, so a typo
    /// cannot trigger a real mode switch.
    fn from_str(vendor: &str) -> Result<Self, Self::Err> {
        match vendor {
            "integrated" => Ok(GraphicsMode::Integrated),
            "nvidia" => Ok(GraphicsMode::Discrete),
            "hybrid" => Ok(GraphicsMode::Hybrid),
            "compute" => Ok(GraphicsMode::Compute),
            other => Err(InvalidGraphicsMode(other.to_owned())),
        }
    }
}

impl TryFrom<&str> for GraphicsMode {
    type Error = InvalidGraphicsMode;

    fn try_from(vendor: &str) -> Result<Self, Self::Error> { vendor.parse() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_mode_name_round_trip() {
        for mode in [
            GraphicsMode::Integrated,
            GraphicsMode::Compute,
            GraphicsMode::Hybrid,
            GraphicsMode::Discrete,
        ] {
            let name: &'static str = mode.into();
            assert_eq!(name.parse::<GraphicsMode>(), Ok(mode));
        }
    }

    #[test]
    fn rejects_unknown_names() {
        for bad in ["", "intergrated", "Integrated", "discrete", "nvidia "] {
            assert_eq!(
                bad.parse::<GraphicsMode>(),
                Err(InvalidGraphicsMode(bad.to_owned())),
                "{:?} must not parse",
                bad
            );
        }
    }
}
