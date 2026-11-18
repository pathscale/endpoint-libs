#![cfg_attr(not(any(test, feature = "std")), no_std)]
#![cfg_attr(test, allow(deprecated))]

extern crate alloc;

#[forbid(unsafe_code)]
#[doc(hidden)]
pub mod convert_case {
    use alloc::string::String;
    use alloc::vec::Vec;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Case {
        Snake,
        Camel,
        Pascal,
        ScreamingSnake,
        UpperSnake,
        Kebab,
    }

    fn split_words(input: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut current = String::new();
        let chars: Vec<char> = input.chars().collect();
        for i in 0..chars.len() {
            let ch = chars[i];
            if !ch.is_alphanumeric() {
                if !current.is_empty() {
                    words.push(core::mem::take(&mut current));
                }
                continue;
            }
            if i > 0 && ch.is_uppercase() {
                let prev = chars[i - 1];
                let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
                if prev.is_lowercase() || prev.is_numeric() || (prev.is_uppercase() && next_lower) {
                    if !current.is_empty() {
                        words.push(core::mem::take(&mut current));
                    }
                }
            }
            current.push(ch);
        }
        if !current.is_empty() {
            words.push(current);
        }
        words
    }

    fn capitalize(word: &str) -> String {
        let mut chars = word.chars();
        match chars.next() {
            None => String::new(),
            Some(first) => {
                let mut out = String::new();
                for c in first.to_uppercase() {
                    out.push(c);
                }
                for c in chars {
                    for lc in c.to_lowercase() {
                        out.push(lc);
                    }
                }
                out
            }
        }
    }

    fn lowercase(word: &str) -> String {
        let mut out = String::new();
        for c in word.chars() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
        }
        out
    }

    fn uppercase(word: &str) -> String {
        let mut out = String::new();
        for c in word.chars() {
            for uc in c.to_uppercase() {
                out.push(uc);
            }
        }
        out
    }

    pub trait Casing {
        fn to_case(&self, case: Case) -> String;
    }

    impl<T: AsRef<str> + ?Sized> Casing for T {
        fn to_case(&self, case: Case) -> String {
            let words = split_words(self.as_ref());
            match case {
                Case::Snake => {
                    let mut out = String::new();
                    for (i, w) in words.iter().enumerate() {
                        if i > 0 {
                            out.push('_');
                        }
                        out.push_str(&lowercase(w));
                    }
                    out
                }
                Case::ScreamingSnake | Case::UpperSnake => {
                    let mut out = String::new();
                    for (i, w) in words.iter().enumerate() {
                        if i > 0 {
                            out.push('_');
                        }
                        out.push_str(&uppercase(w));
                    }
                    out
                }
                Case::Kebab => {
                    let mut out = String::new();
                    for (i, w) in words.iter().enumerate() {
                        if i > 0 {
                            out.push('-');
                        }
                        out.push_str(&lowercase(w));
                    }
                    out
                }
                Case::Pascal => {
                    let mut out = String::new();
                    for w in &words {
                        out.push_str(&capitalize(w));
                    }
                    out
                }
                Case::Camel => {
                    let mut out = String::new();
                    for (i, w) in words.iter().enumerate() {
                        if i == 0 {
                            out.push_str(&lowercase(w));
                        } else {
                            out.push_str(&capitalize(w));
                        }
                    }
                    out
                }
            }
        }
    }
}

#[forbid(unsafe_code)]
#[doc(hidden)]
pub mod eyre {
    use alloc::boxed::Box;
    use alloc::format;
    use alloc::string::ToString;
    use core::any::Any;
    use core::fmt::{self, Debug, Display};

    pub type Result<T, E = Report> = core::result::Result<T, E>;
    pub type Error = Report;

    pub trait EyreHandler: Any + Send + Sync {
        fn display(
            &self,
            error: &(dyn core::error::Error + 'static),
            f: &mut fmt::Formatter<'_>,
        ) -> fmt::Result;

        fn debug(
            &self,
            error: &(dyn core::error::Error + 'static),
            f: &mut fmt::Formatter<'_>,
        ) -> fmt::Result;

        fn track_caller(&mut self, _location: &'static core::panic::Location<'static>) {}
    }

    pub struct DefaultHandler;

    impl DefaultHandler {
        pub fn default_with(_error: &(dyn core::error::Error + 'static)) -> Box<dyn EyreHandler> {
            Box::new(DefaultHandler)
        }
    }

    impl EyreHandler for DefaultHandler {
        fn display(
            &self,
            error: &(dyn core::error::Error + 'static),
            f: &mut fmt::Formatter<'_>,
        ) -> fmt::Result {
            Display::fmt(error, f)
        }

        fn debug(
            &self,
            error: &(dyn core::error::Error + 'static),
            f: &mut fmt::Formatter<'_>,
        ) -> fmt::Result {
            Debug::fmt(error, f)
        }
    }

    struct MessageError<M>(M);

    impl<M: Display> Display for MessageError<M> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            Display::fmt(&self.0, f)
        }
    }

    impl<M: Debug> Debug for MessageError<M> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            Debug::fmt(&self.0, f)
        }
    }

    impl<M: Display + Debug> core::error::Error for MessageError<M> {}

    pub struct Report {
        inner: Box<dyn core::error::Error + Send + Sync + 'static>,
        handler: Box<dyn EyreHandler>,
    }

    impl Report {
        #[track_caller]
        pub fn new<E>(error: E) -> Self
        where
            E: core::error::Error + Send + Sync + 'static,
        {
            let location = core::panic::Location::caller();
            let inner: Box<dyn core::error::Error + Send + Sync + 'static> =
                Box::new(MessageError(error));
            let mut handler = DefaultHandler::default_with(&*inner);
            handler.track_caller(location);
            Self { inner, handler }
        }

        #[track_caller]
        pub fn msg<M>(message: M) -> Self
        where
            M: Display + Debug + Send + Sync + 'static,
        {
            let location = core::panic::Location::caller();
            let inner: Box<dyn core::error::Error + Send + Sync + 'static> =
                Box::new(MessageError(message));
            let mut handler = DefaultHandler::default_with(&*inner);
            handler.track_caller(location);
            Self { inner, handler }
        }

        pub fn wrap_err<D>(self, msg: D) -> Self
        where
            D: Display + Send + Sync + 'static,
        {
            Self::msg(format!("{msg}: {}", self.inner))
        }

        pub fn handler(&self) -> &(dyn Any + Send + Sync) {
            &*self.handler
        }

        pub fn is<E: Display + Debug + Send + Sync + 'static>(&self) -> bool {
            self.downcast_ref::<E>().is_some()
        }

        pub fn downcast_ref<E: Display + Debug + Send + Sync + 'static>(&self) -> Option<&E> {
            if let Some(wrapped) = <dyn core::error::Error + Send + Sync + 'static>::downcast_ref::<
                MessageError<E>,
            >(&*self.inner)
            {
                return Some(&wrapped.0);
            }
            None
        }

        pub fn downcast<E: Display + Debug + Send + Sync + 'static>(
            self,
        ) -> core::result::Result<E, Self> {
            let Self { inner, handler } = self;
            match inner.downcast::<MessageError<E>>() {
                Ok(b) => Ok(b.0),
                Err(inner) => Err(Self { inner, handler }),
            }
        }
    }

    impl AsRef<dyn core::error::Error + 'static> for Report {
        fn as_ref(&self) -> &(dyn core::error::Error + 'static) {
            &*self.inner
        }
    }

    impl core::ops::Deref for Report {
        type Target = dyn core::error::Error + Send + Sync + 'static;

        fn deref(&self) -> &Self::Target {
            &*self.inner
        }
    }

    impl Display for Report {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.handler.display(&*self.inner, f)
        }
    }

    impl Debug for Report {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.handler.debug(&*self.inner, f)
        }
    }

    impl<E> From<E> for Report
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        #[track_caller]
        fn from(error: E) -> Self {
            Self::new(error)
        }
    }

    pub trait WrapErr<T, E> {
        fn wrap_err<D>(self, msg: D) -> Result<T, Report>
        where
            D: Display + Send + Sync + 'static;

        fn wrap_err_with<D, F>(self, f: F) -> Result<T, Report>
        where
            D: Display + Send + Sync + 'static,
            F: FnOnce() -> D;

        fn context<D>(self, msg: D) -> Result<T, Report>
        where
            D: Display + Send + Sync + 'static,
            Self: Sized,
        {
            self.wrap_err(msg)
        }

        fn with_context<D, F>(self, f: F) -> Result<T, Report>
        where
            D: Display + Send + Sync + 'static,
            F: FnOnce() -> D,
            Self: Sized,
        {
            self.wrap_err_with(f)
        }
    }

    pub use WrapErr as Context;

    impl<T, E> WrapErr<T, E> for core::result::Result<T, E>
    where
        E: Into<Report>,
    {
        #[track_caller]
        fn wrap_err<D>(self, msg: D) -> Result<T, Report>
        where
            D: Display + Send + Sync + 'static,
        {
            match self {
                Ok(v) => Ok(v),
                Err(e) => Err(e.into().wrap_err(msg)),
            }
        }

        #[track_caller]
        fn wrap_err_with<D, F>(self, f: F) -> Result<T, Report>
        where
            D: Display + Send + Sync + 'static,
            F: FnOnce() -> D,
        {
            match self {
                Ok(v) => Ok(v),
                Err(e) => Err(e.into().wrap_err(f())),
            }
        }
    }

    pub trait ContextCompat<T> {
        fn context<D>(self, msg: D) -> Result<T, Report>
        where
            D: Display + Send + Sync + 'static;

        fn with_context<D, F>(self, f: F) -> Result<T, Report>
        where
            D: Display + Send + Sync + 'static,
            F: FnOnce() -> D;

        fn wrap_err<D>(self, msg: D) -> Result<T, Report>
        where
            D: Display + Send + Sync + 'static,
            Self: Sized,
        {
            self.context(msg)
        }

        fn with_context_compat<D, F>(self, f: F) -> Result<T, Report>
        where
            D: Display + Send + Sync + 'static,
            F: FnOnce() -> D,
            Self: Sized,
        {
            self.with_context(f)
        }

        fn wrap_err_with<D, F>(self, f: F) -> Result<T, Report>
        where
            D: Display + Send + Sync + 'static,
            F: FnOnce() -> D,
            Self: Sized,
        {
            self.with_context(f)
        }
    }

    impl<T> ContextCompat<T> for Option<T> {
        #[track_caller]
        fn context<D>(self, msg: D) -> Result<T, Report>
        where
            D: Display + Send + Sync + 'static,
        {
            match self {
                Some(v) => Ok(v),
                None => Err(Report::msg(msg.to_string())),
            }
        }

        #[track_caller]
        fn with_context<D, F>(self, f: F) -> Result<T, Report>
        where
            D: Display + Send + Sync + 'static,
            F: FnOnce() -> D,
        {
            match self {
                Some(v) => Ok(v),
                None => Err(Report::msg(f().to_string())),
            }
        }
    }

    pub fn format_msg(args: fmt::Arguments<'_>) -> Report {
        Report::msg(alloc::fmt::format(args))
    }

    pub use crate::{
        __endpoint_libs_bail as bail, __endpoint_libs_ensure as ensure,
        __endpoint_libs_eyre as eyre,
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __endpoint_libs_eyre {
    ($msg:literal $(,)?) => {
        $crate::eyre::format_msg(::core::format_args!($msg))
    };
    ($err:expr $(,)?) => {
        $crate::eyre::format_msg(::core::format_args!("{}", $err))
    };
    ($fmt:expr, $($arg:tt)*) => {
        $crate::eyre::format_msg(::core::format_args!($fmt, $($arg)*))
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __endpoint_libs_bail {
    ($($tt:tt)*) => {
        return ::core::result::Result::Err($crate::__endpoint_libs_eyre!($($tt)*))
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __endpoint_libs_ensure {
    ($cond:expr, $($tt:tt)*) => {
        if !($cond) {
            $crate::__endpoint_libs_bail!($($tt)*);
        }
    };
    ($cond:expr) => {
        if !($cond) {
            $crate::__endpoint_libs_bail!(concat!("Condition failed: `", stringify!($cond), "`"));
        }
    };
}

#[forbid(unsafe_code)]
#[doc(hidden)]
pub mod urlencoding {
    use alloc::borrow::Cow;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::fmt;

    #[derive(Debug)]
    pub struct FromUrlEncodingError;

    impl fmt::Display for FromUrlEncodingError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("invalid percent-encoded UTF-8 sequence")
        }
    }

    impl core::error::Error for FromUrlEncodingError {}

    pub fn encode(data: &str) -> Cow<'_, str> {
        let bytes = data.as_bytes();
        let safe = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if bytes.iter().copied().all(safe) {
            return Cow::Borrowed(data);
        }
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let mut out = String::with_capacity(bytes.len() * 3);
        for &b in bytes {
            if safe(b) {
                out.push(b as char);
            } else {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
        Cow::Owned(out)
    }

    pub fn decode(data: &str) -> Result<Cow<'_, str>, FromUrlEncodingError> {
        let bytes = data.as_bytes();
        if !bytes.contains(&b'%') {
            return Ok(Cow::Borrowed(data));
        }
        let hex = |b: u8| -> Result<u8, FromUrlEncodingError> {
            match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                b'A'..=b'F' => Ok(b - b'A' + 10),
                _ => Err(FromUrlEncodingError),
            }
        };
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                if i + 2 >= bytes.len() {
                    return Err(FromUrlEncodingError);
                }
                out.push((hex(bytes[i + 1])? << 4) | hex(bytes[i + 2])?);
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out)
            .map(Cow::Owned)
            .map_err(|_| FromUrlEncodingError)
    }
}

pub mod libs;
#[cfg(feature = "types")]
pub mod model;
