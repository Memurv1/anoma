//! Command line interface utilities
use std::fmt::Debug;
use std::marker::PhantomData;
use std::str::FromStr;

use clap::ArgMatches;

use super::{args, input, Context};
use crate::wallet::Wallet;

// We only use static strings
pub type App = clap::App<'static>;
pub type ClapArg = clap::Arg<'static>;

pub trait Cmd: Sized {
    fn add_sub(app: App) -> App;
    fn parse(ctx: &Context, matches: &ArgMatches) -> Option<Self>;

    fn parse_or_print_help(app: App) -> (Self, Context) {
        let mut app = Self::add_sub(app);
        let matches = app.clone().get_matches();
        let global_args = args::Global::parse(&matches);
        let wallet = Wallet::load_or_new(&global_args.base_dir);
        let context = Context {
            global_args,
            wallet,
        };
        let result = Self::parse(&context, &matches);
        match result {
            Some(cmd) => (cmd, context),
            None => {
                app.print_help().unwrap();
                safe_exit(2);
            }
        }
    }
}

pub trait SubCmd: Sized {
    const CMD: &'static str;
    fn parse(ctx: &Context, matches: &ArgMatches) -> Option<Self>;
    fn def() -> App;
}

pub trait Args {
    fn parse(ctx: &Context, matches: &ArgMatches) -> Self;
    fn def(app: App) -> App;
}

pub struct Arg<T> {
    pub name: &'static str,
    pub r#type: PhantomData<T>,
}

pub struct ArgOpt<T> {
    pub name: &'static str,
    pub r#type: PhantomData<T>,
}

pub struct ArgDefault<T> {
    pub name: &'static str,
    pub default: DefaultFn<T>,
    pub r#type: PhantomData<T>,
}

/// This wrapper type is a workaround for "function pointers in const fn are
/// unstable", which allows us to use this type in a const fn, because the
/// type-checker doesn't inspect the wrapped type.
/// Const function pointers: <https://github.com/rust-lang/rust/issues/63997>.
pub struct DefaultFn<T>(pub fn() -> T);

pub struct ArgFlag {
    pub name: &'static str,
}

pub struct ArgMulti<T> {
    pub name: &'static str,
    pub r#type: PhantomData<T>,
}

pub const fn arg<T>(name: &'static str) -> Arg<T> {
    Arg {
        name,
        r#type: PhantomData,
    }
}

pub const fn arg_opt<T>(name: &'static str) -> ArgOpt<T> {
    ArgOpt {
        name,
        r#type: PhantomData,
    }
}

pub const fn arg_default<T>(
    name: &'static str,
    default: DefaultFn<T>,
) -> ArgDefault<T> {
    ArgDefault {
        name,
        default,
        r#type: PhantomData,
    }
}

pub const fn flag(name: &'static str) -> ArgFlag {
    ArgFlag { name }
}

pub const fn arg_multi<T>(name: &'static str) -> ArgMulti<T> {
    ArgMulti {
        name,
        r#type: PhantomData,
    }
}

impl<T> Arg<T> {
    pub const fn opt(self) -> ArgOpt<T> {
        ArgOpt {
            name: self.name,
            r#type: PhantomData,
        }
    }

    pub const fn default(self, default: DefaultFn<T>) -> ArgDefault<T> {
        ArgDefault {
            name: self.name,
            default,
            r#type: PhantomData,
        }
    }

    pub const fn multi(self) -> ArgMulti<T> {
        ArgMulti {
            name: self.name,
            r#type: PhantomData,
        }
    }
}

impl<T> Arg<T>
where
    T: input::ArgInput,
    <T as input::ArgInput>::Err: Debug,
{
    pub fn def(&self) -> ClapArg {
        ClapArg::new(self.name)
            .long(self.name)
            .takes_value(true)
            .required(true)
    }

    pub fn parse(&self, ctx: &Context, matches: &ArgMatches) -> T {
        parse_opt(ctx, matches, self.name).unwrap()
    }
}

impl<T> ArgOpt<T>
where
    T: input::ArgInput,
    <T as input::ArgInput>::Err: Debug,
{
    pub fn def(&self) -> ClapArg {
        ClapArg::new(self.name).long(self.name).takes_value(true)
    }

    pub fn parse(&self, ctx: &Context, matches: &ArgMatches) -> Option<T> {
        parse_opt(ctx, matches, self.name)
    }
}

impl<T> ArgDefault<T> {
    pub fn def(&self) -> ClapArg {
        ClapArg::new(self.name).long(self.name).takes_value(true)
    }
}

impl<T> ArgDefault<T>
where
    T: FromStr,
    <T as FromStr>::Err: Debug,
{
    /// Global arguments don't have context, because context is created from
    /// global args.
    pub fn parse_global(&self, matches: &ArgMatches) -> T {
        matches
            .value_of(self.name)
            .and_then(|arg| arg.parse().ok())
            .unwrap_or_else(|| {
                let DefaultFn(default) = self.default;
                default()
            })
    }
}

impl<T> ArgDefault<T>
where
    T: input::ArgInput,
    <T as input::ArgInput>::Err: Debug,
{
    /// Parse and argument and look-up a values from the context, if necessary
    /// (e.g. to find a key from an alias in the wallet).
    pub fn parse(&self, ctx: &Context, matches: &ArgMatches) -> T {
        parse_opt(ctx, matches, self.name).unwrap_or_else(|| {
            let DefaultFn(default) = self.default;
            default()
        })
    }
}

impl ArgFlag {
    pub fn def(&self) -> ClapArg {
        ClapArg::new(self.name).long(self.name).takes_value(false)
    }

    pub fn parse(&self, matches: &ArgMatches) -> bool {
        matches.is_present(self.name)
    }
}

impl<T> ArgMulti<T>
where
    T: input::ArgInput,
    <T as input::ArgInput>::Err: Debug,
{
    pub fn def(&self) -> ClapArg {
        ClapArg::new(self.name).long(self.name).multiple(true)
    }

    pub fn parse(&self, ctx: &Context, matches: &ArgMatches) -> Vec<T> {
        matches
            .values_of(self.name)
            .unwrap_or_default()
            .map(|raw| {
                T::try_from_raw(ctx, raw).unwrap_or_else(|err| {
                    panic!(
                        "Failed to parse the {} argument. Raw value: {}, \
                         error {}",
                        self.name, raw, err
                    )
                })
            })
            .collect()
    }
}

/// Extensions for defining commands and arguments.
/// Every function here should have a matcher in [`ArgMatchesExt`].
pub trait AppExt {
    fn add_args<T: Args>(self) -> Self;
}

/// Extensions for finding matching commands and arguments.
/// The functions match commands and arguments defined in [`AppExt`].
pub trait ArgMatchesExt {
    fn args_parse<T: Args>(&self, ctx: &Context) -> T;
}

impl AppExt for App {
    fn add_args<T: Args>(self) -> Self {
        T::def(self)
    }
}

impl ArgMatchesExt for ArgMatches {
    fn args_parse<T: Args>(&self, ctx: &Context) -> T {
        T::parse(ctx, self)
    }
}

pub fn parse_opt<T>(ctx: &Context, args: &ArgMatches, field: &str) -> Option<T>
where
    T: input::ArgInput,
    T::Err: Debug,
{
    args.value_of(field).map(|arg| {
        T::try_from_raw(ctx, arg).unwrap_or_else(|e| {
            panic!(
                "Failed to parse the argument {}. Raw value: {}, error: {}",
                field, arg, e
            )
        })
    })
}

/// A helper to exit after flushing output, borrowed from `clap::util` module.
pub fn safe_exit(code: i32) -> ! {
    use std::io::Write;

    let _ = std::io::stdout().lock().flush();
    let _ = std::io::stderr().lock().flush();

    std::process::exit(code)
}
