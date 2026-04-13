use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scale {
    Small,
    Medium,
    Large,
}

impl Scale {
    pub fn entity_count(self) -> usize {
        match self {
            Scale::Small => 500,
            Scale::Medium => 5_000,
            Scale::Large => 50_000,
        }
    }

    pub fn claim_count(self) -> usize {
        match self {
            Scale::Small => 2_000,
            Scale::Medium => 20_000,
            Scale::Large => 200_000,
        }
    }

    pub fn relation_count(self) -> usize {
        match self {
            Scale::Small => 1_000,
            Scale::Medium => 10_000,
            Scale::Large => 100_000,
        }
    }

    pub fn embedding_count(self) -> usize {
        match self {
            Scale::Small => 800,
            Scale::Medium => 8_000,
            Scale::Large => 80_000,
        }
    }

    pub fn all() -> &'static [Scale] {
        &[Scale::Small, Scale::Medium, Scale::Large]
    }

    pub fn from_env() -> Scale {
        match std::env::var("BENCH_SCALE").as_deref() {
            Ok("small") => Scale::Small,
            Ok("medium") => Scale::Medium,
            Ok("large") => Scale::Large,
            _ => Scale::Small,
        }
    }
}

impl fmt::Display for Scale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scale::Small => write!(f, "small"),
            Scale::Medium => write!(f, "medium"),
            Scale::Large => write!(f, "large"),
        }
    }
}
