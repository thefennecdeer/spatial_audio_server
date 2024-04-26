use macro_attr_2018::macro_attr;
use newtype_derive_2018::*;
macro_attr! {

    #[derive(Copy, Clone, Debug, Default, PartialEq, PartialOrd,
             NewtypeFrom!,
             NewtypeAdd!, NewtypeSub!, NewtypeMul!, NewtypeMul!(f64), NewtypeDiv!, NewtypeDiv!(f64),
             NewtypeRem!,
             NewtypeNeg!)]
    #[derive(Serialize, Deserialize)]
    pub struct Metres(pub f64);
}

impl Metres {
    pub fn min(self, other: Self) -> Self {
        if other < self {
            other
        } else {
            self
        }
    }

    pub fn max(self, other: Self) -> Self {
        if other > self {
            other
        } else {
            self
        }
    }
}
