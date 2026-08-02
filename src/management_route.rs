#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ManagementRouteMode {
    #[default]
    AutomaticLan,

    DirectLink,

    ExplicitIp,
}

impl ManagementRouteMode {
    pub const fn code(self) -> u8 {
        match self {
            Self::AutomaticLan => 0,
            Self::DirectLink => 1,
            Self::ExplicitIp => 2,
        }
    }

    pub const fn from_code(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::AutomaticLan),
            1 => Some(Self::DirectLink),
            2 => Some(Self::ExplicitIp),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::AutomaticLan => "Automatic LAN",
            Self::DirectLink => "Direct Link",
            Self::ExplicitIp => "Explicit IP",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::AutomaticLan => "Use management agents discovered on the local network.",

            Self::DirectLink => {
                "Use management agents reachable through a dedicated direct Ethernet link."
            }

            Self::ExplicitIp => {
                "Use the sender and receiver management addresses exactly as entered."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ManagementRouteMode;

    #[test]
    fn route_codes_round_trip() {
        for expected in [
            ManagementRouteMode::AutomaticLan,
            ManagementRouteMode::DirectLink,
            ManagementRouteMode::ExplicitIp,
        ] {
            assert_eq!(
                ManagementRouteMode::from_code(expected.code()),
                Some(expected),
            );
        }

        assert_eq!(ManagementRouteMode::from_code(3), None);
    }

    #[test]
    fn automatic_lan_is_the_safe_legacy_default() {
        assert_eq!(
            ManagementRouteMode::default(),
            ManagementRouteMode::AutomaticLan,
        );
    }
}
