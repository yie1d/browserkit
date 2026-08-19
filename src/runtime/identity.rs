macro_rules! opaque_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

opaque_id!(RuntimeId);
opaque_id!(BrowserSessionId);
opaque_id!(PageId);
opaque_id!(FrameId);

#[cfg(test)]
mod tests {
    use super::*;
    use static_assertions::assert_impl_all;

    assert_impl_all!(RuntimeId: Clone, Eq, std::hash::Hash, Send, Sync);
    assert_impl_all!(BrowserSessionId: Clone, Eq, std::hash::Hash, Send, Sync);
    assert_impl_all!(PageId: Clone, Eq, std::hash::Hash, Send, Sync);
    assert_impl_all!(FrameId: Clone, Eq, std::hash::Hash, Send, Sync);

    fn assert_static<T: 'static>() {}

    #[test]
    fn identities_preserve_their_opaque_values() {
        assert_static::<RuntimeId>();
        assert_static::<BrowserSessionId>();
        assert_static::<PageId>();
        assert_static::<FrameId>();
        assert_eq!(RuntimeId::new("runtime-1").as_str(), "runtime-1");
        assert_eq!(BrowserSessionId::new("session-1").as_str(), "session-1");
        assert_eq!(PageId::new("page-1").as_str(), "page-1");
        assert_eq!(FrameId::new("frame-1").as_str(), "frame-1");
    }
}
