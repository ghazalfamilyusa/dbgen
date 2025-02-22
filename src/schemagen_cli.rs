//! CLI driver of `dbschemagen`.

// ALLOW_REASON: this package is full of math that does not require full precision.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use crate::{error::Error, parser::QName};
use clap::{Parser, ValueEnum};
use rand::{Rng, RngCore, rngs::OsRng, seq::SliceRandom};
use rand_distr::{Distribution, Geometric, LogNormal, Pareto, weighted_alias::WeightedAliasIndex};
use std::{
    collections::{BTreeSet, HashSet},
    f64::consts::LOG2_10,
    fmt::Write,
    iter::{once, repeat_with},
    mem::replace,
    str::FromStr,
};

/// Arguments to the `dbschemagen` CLI program.
#[derive(Parser, Debug)]
#[command(long_version(crate::FULL_VERSION), next_line_help(true))]
pub struct Args {
    /// Schema name.
    #[arg(short, long)]
    pub schema_name: String,

    /// Estimated total database dump size in bytes.
    #[arg(short = 'z', long)]
    pub size: f64,

    /// Number of tables to generate
    #[arg(short, long)]
    pub tables_count: u32,

    /// SQL dialect.
    #[arg(short, long, value_enum)]
    pub dialect: Dialect,

    /// Number of INSERT statements per file.
    #[arg(short = 'n', long, default_value = "1000")]
    pub inserts_count: u64,

    /// Number of rows per INSERT statement.
    #[arg(short, long, default_value = "100")]
    pub rows_count: u64,

    /// Random number generator seed (should have 64 hex digits).
    #[arg(long)]
    pub seed: Option<crate::cli::Seed>,

    /// Additional arguments passed to every `dbgen` invocation
    #[arg(trailing_var_arg(true))]
    pub args: Vec<String>,
}

/// The SQL dialect used when generating the schemas.
#[derive(Debug, Copy, Clone, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Dialect {
    /// MySQL dialect.
    MySQL,
    /// PostgreSQL dialect.
    PostgreSQL,
    /// SQLite dialect.
    SQLite,
}

impl FromStr for Dialect {
    type Err = Error;
    fn from_str(dialect: &str) -> Result<Self, Self::Err> {
        Ok(match dialect {
            "mysql" => Self::MySQL,
            "postgresql" => Self::PostgreSQL,
            "sqlite" => Self::SQLite,
            _ => {
                return Err(Error::UnsupportedCliParameter {
                    kind: "SQL dialect",
                    value: dialect.to_owned(),
                });
            }
        })
    }
}

struct Column {
    /// The column type.
    ty: String,
    /// `dbgen` expression to generate a value of this type.
    expr: String,
    /// The -log₂(probability) which two randomly generated values will collide (assuming perfect RNG).
    neg_log2_prob: f64,
    /// The estimated average formatted length a generated value of this column.
    average_len: f64,
    /// Whether the type is nullable (thus must be excluded from PRIMARY KEY).
    nullable: bool,
}

type ColumnGenerator = fn(Dialect, &mut dyn RngCore) -> Column;

fn gen_int_column(dialect: Dialect, rng: &mut dyn RngCore) -> Column {
    let bytes = rng.gen_range(0..8);
    let unsigned = rng.r#gen::<bool>();
    // ALLOW_REASON: different database engines sharing the same name for different type
    // does not mean we should combine them into the same arm.
    #[allow(clippy::match_same_arms)]
    let ty = match (dialect, unsigned, bytes) {
        (Dialect::MySQL, false, 0) => "tinyint",
        (Dialect::MySQL, false, 1) => "smallint",
        (Dialect::MySQL, false, 2) => "mediumint",
        (Dialect::MySQL, false, 3) => "int",
        (Dialect::MySQL, false, _) => "bigint",
        (Dialect::MySQL, true, 0) => "tinyint unsigned",
        (Dialect::MySQL, true, 1) => "smallint unsigned",
        (Dialect::MySQL, true, 2) => "mediumint unsigned",
        (Dialect::MySQL, true, 3) => "int unsigned",
        (Dialect::MySQL, true, _) => "bigint unsigned",
        (Dialect::PostgreSQL, false, 0..=1) => "smallint",
        (Dialect::PostgreSQL, false, 2..=3) => "int",
        (Dialect::PostgreSQL, false, _) => "bigint",
        (Dialect::PostgreSQL, true, 0) => "smallint",
        (Dialect::PostgreSQL, true, 1..=2) => "int",
        (Dialect::PostgreSQL, true, 3..=6) => "bigint",
        (Dialect::PostgreSQL, true, _) => "numeric(20)",
        (Dialect::SQLite, _, _) => "integer",
    };
    let ty = format!("{ty} not null");
    let (min, max) = if unsigned {
        (0, (256 << (8 * bytes)) - 1)
    } else {
        let base = 128_i128 << (8 * bytes);
        (-base, base - 1)
    };
    let neg_log2_prob = f64::from(bytes + 1) * 8.0;

    let end = (max + 1) as f64;
    let digits = end.log10().ceil();
    let mut average_len = digits - (10_f64.powf(digits) - 10.0) / (9.0 * end);
    if !unsigned {
        average_len = average_len * 2.0 + 1.0;
    }

    Column {
        ty,
        expr: format!("rand.range_inclusive({min}, {max})"),
        neg_log2_prob,
        average_len,
        nullable: false,
    }
}

fn gen_serial_column(dialect: Dialect, _: &mut dyn RngCore) -> Column {
    let ty = match dialect {
        Dialect::MySQL => "bigint unsigned not null",
        Dialect::PostgreSQL => "bigserial",
        Dialect::SQLite => "integer not null",
    };
    Column {
        ty: ty.to_owned(),
        expr: "rownum".to_owned(),
        neg_log2_prob: 64.0,
        average_len: 6.0,
        nullable: false,
    }
}

fn gen_decimal_column(_: Dialect, rng: &mut dyn RngCore) -> Column {
    let before = rng.gen_range(1_u8..19);
    let after = rng.gen_range(0_u8..31);
    let limit = "9".repeat(usize::from(before));
    Column {
        ty: format!("decimal({}, {}) not null", before + after, after),
        expr: format!("rand.range_inclusive(-{limit}, {limit}) || rand.regex('\\.[0-9]{{{after}}}')"),
        neg_log2_prob: LOG2_10 * f64::from(before + after) + 1.0,
        average_len: f64::from(before + after) + 17.0 / 9.0,
        nullable: false,
    }
}

// 4382594 / 1112064
const AVERAGE_LEN_PER_CHAR: f64 = 3.940_954_837_131_676;
const VALID_CHARS_COUNT: f64 = 1_112_064.0;

fn gen_varchar_column(_: Dialect, rng: &mut dyn RngCore) -> Column {
    let len = rng.gen_range(1..=255);
    let residue = (VALID_CHARS_COUNT / (VALID_CHARS_COUNT - 1.0)).log2();
    Column {
        ty: format!("varchar({len}) not null"),
        expr: format!("rand.regex('.{{0,{len}}}', 's')"),
        neg_log2_prob: f64::from(len + 1).log2() - residue,
        average_len: AVERAGE_LEN_PER_CHAR * 0.5 * f64::from(len) + 2.0,
        nullable: false,
    }
}

fn gen_char_column(_: Dialect, rng: &mut dyn RngCore) -> Column {
    let len = rng.gen_range(1..=255);
    let factor = VALID_CHARS_COUNT.log2();
    Column {
        ty: format!("char({len}) not null"),
        expr: format!("rand.regex('.{{{len}}}', 's')"),
        neg_log2_prob: factor * f64::from(len),
        average_len: AVERAGE_LEN_PER_CHAR * f64::from(len) + 2.0,
        nullable: false,
    }
}

fn gen_timestamp_column(dialect: Dialect, _: &mut dyn RngCore) -> Column {
    let ty = match dialect {
        Dialect::SQLite => "text not null",
        Dialect::MySQL | Dialect::PostgreSQL => "timestamp not null",
    };
    Column {
        ty: ty.to_owned(),
        expr: "rand.u31_timestamp()".to_owned(),
        neg_log2_prob: 31.0,
        average_len: 21.0,
        nullable: false,
    }
}

const DATEIME_SECONDS: f64 = 284_012_524_800_f64;

fn gen_datetime_column(dialect: Dialect, _: &mut dyn RngCore) -> Column {
    let ty = match dialect {
        Dialect::SQLite => "text not null",
        Dialect::MySQL => "datetime not null",
        Dialect::PostgreSQL => "timestamp not null",
    };
    Column {
        ty: ty.to_owned(),
        expr: "TIMESTAMP '1000-01-01 00:00:00' + INTERVAL rand.range(0, 284012524800) SECOND".to_owned(),
        neg_log2_prob: DATEIME_SECONDS.log2(),
        average_len: 21.0,
        nullable: false,
    }
}

fn gen_nullable_bool_column(_: Dialect, rng: &mut dyn RngCore) -> Column {
    let p = rng.r#gen::<f64>();
    Column {
        ty: "boolean".to_owned(),
        expr: format!("CASE WHEN rand.bool({p}) THEN '' || rand.bool(0.5) END"),
        neg_log2_prob: -((1.5 * p - 2.0) * p + 1.0).log2(),
        average_len: 4.0 - p,
        nullable: true,
    }
}

const NEG_LOG2_PROB_FINITE_F32: f64 = 31.994_353_436_858_86;
const NEG_LOG2_PROB_FINITE_F64: f64 = 63.999_295_387_023_41;

fn gen_float_column(dialect: Dialect, rng: &mut dyn RngCore) -> Column {
    let bits = rng.gen_range(1..=2) * 32;
    let ty = match (bits, dialect) {
        (32, Dialect::MySQL) => "float not null",
        (64, Dialect::MySQL) => "double not null",
        (64, Dialect::PostgreSQL) => "double precision not null",
        _ => "real not null",
    };
    Column {
        ty: ty.to_owned(),
        expr: format!("rand.finite_f{bits}()"),
        neg_log2_prob: if bits == 32 {
            NEG_LOG2_PROB_FINITE_F32
        } else {
            NEG_LOG2_PROB_FINITE_F64
        },
        average_len: 21.966,
        nullable: false,
    }
}

static GENERATORS: [ColumnGenerator; 9] = [
    gen_int_column,
    gen_serial_column,
    gen_varchar_column,
    gen_char_column,
    gen_timestamp_column,
    gen_datetime_column,
    gen_nullable_bool_column,
    gen_decimal_column,
    gen_float_column,
];

fn gen_column(dialect: Dialect, rng: &mut dyn RngCore) -> Column {
    let r#gen = GENERATORS.choose(rng).unwrap();
    r#gen(dialect, rng)
}

struct IndexAppender<'a> {
    index_count_distr: WeightedAliasIndex<f64>,
    index_distr: WeightedAliasIndex<f64>,
    columns: &'a [Column],
    index_sets: HashSet<BTreeSet<usize>>,
}

impl<'a> IndexAppender<'a> {
    fn new(columns: &'a [Column]) -> Self {
        Self {
            // This is equivalent to the Pareto distribution
            // with scale = 1.0, shape = 1.6, truncated at 12.
            index_count_distr: WeightedAliasIndex::new(
                (1_u8..13)
                    .map(|i| f64::from(i).powf(-1.6))
                    .scan(1.0, |prev, pdf| Some(replace(prev, pdf) - pdf))
                    .chain(once(12.0_f64.powf(-1.6)))
                    .collect(),
            )
            .unwrap(),
            index_distr: WeightedAliasIndex::new(columns.iter().map(|col| col.neg_log2_prob.min(32.0)).collect())
                .unwrap(),
            columns,
            index_sets: HashSet::new(),
        }
    }

    fn append_to(
        &mut self,
        schema: &mut String,
        dialect: Dialect,
        mut rng: &mut dyn RngCore,
        unique_cutoff: f64,
        is_primary_key: bool,
    ) {
        let index_count = self.index_count_distr.sample(&mut rng);
        let index_set = (&self.index_distr)
            .sample_iter(&mut rng)
            .take(index_count)
            .collect::<BTreeSet<_>>();

        let total_neg_log2_prob: f64 = index_set.iter().map(|i| self.columns[*i].neg_log2_prob).sum();
        let is_unique = total_neg_log2_prob > unique_cutoff;
        let is_nullable = index_set.iter().any(|i| self.columns[*i].nullable);
        if is_primary_key && (!is_unique || is_nullable) {
            return;
        }

        let index_spec = index_set.iter().map(|i| format!("c{i}")).collect::<Vec<_>>().join(", ");

        if index_set.is_empty() || !self.index_sets.insert(index_set) {
            return;
        }

        if is_primary_key {
            schema.push_str(",\nPRIMARY KEY (");
        } else if is_unique {
            schema.push_str(",\nUNIQUE (");
        } else if dialect == Dialect::MySQL {
            schema.push_str(",\nKEY (");
        } else {
            return;
        }
        schema.push_str(&index_spec);
        schema.push(')');
    }
}

struct Table {
    schema: String,
    target_size: f64,
    rows_count: u64,
    seed: crate::cli::Seed,
}

fn gen_table(dialect: Dialect, rng: &mut dyn RngCore, target_size: f64) -> Table {
    let mut schema = String::from("CREATE TABLE _ (\n");

    let columns_count = (LogNormal::new(2.354_259_469_228_055, 0.75).unwrap().sample(rng) as usize).max(1);
    let columns = {
        let rng2 = &mut *rng;
        repeat_with(move || gen_column(dialect, rng2))
            .take(columns_count)
            .collect::<Vec<_>>()
    };

    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            schema.push_str(",\n");
        }
        write!(&mut schema, "c{} {} {{{{{}}}}}", i, col.ty, col.expr).unwrap();
    }

    let average_len_per_row: f64 = columns.iter().map(|col| col.average_len + 2.0).sum();
    let rows_count = (target_size / average_len_per_row).ceil();

    // if the collision probability of 2 rows is p,
    // the collision probability of N rows is approximately 1 - exp(-0.5 p N^2) for large N and small p.
    // if a column is going to be marked a UNIQUE, we want this probability be less than ~0.005,
    // i.e. -log2(p) > 2log2(N) - log2(~0.01).
    let unique_cutoff = rows_count.log2() * 2.0 + 6.736_593_289_427_474;

    // pick a random column as primary key
    let mut appender = IndexAppender::new(&columns);
    appender.append_to(&mut schema, dialect, rng, unique_cutoff, true);
    let p = (appender.index_sets.len() as f64) / ((columns_count + appender.index_sets.len()) as f64);
    let secondary_keys_count = Geometric::new(p).unwrap().sample(rng);
    for _ in 0..secondary_keys_count {
        appender.append_to(&mut schema, dialect, rng, unique_cutoff, false);
    }
    schema.push_str("\n);");

    Table {
        schema,
        target_size,
        rows_count: (rows_count as u64).max(1),
        seed: rng.r#gen(),
    }
}

fn gen_tables<'a>(
    dialect: Dialect,
    mut rng: impl Rng + 'a,
    total_target_size: f64,
    tables_count: u32,
) -> impl Iterator<Item = Table> + 'a {
    let distr = Pareto::new(1.0, 1.16).unwrap();
    let relative_sizes = distr
        .sample_iter(&mut rng)
        .take(tables_count as usize)
        .map(|v| v - 1.0)
        .collect::<Vec<_>>();
    let total_relative_size: f64 = relative_sizes.iter().sum();
    let ratio = total_target_size / total_relative_size;
    relative_sizes
        .into_iter()
        .map(move |f| gen_table(dialect, &mut rng, f * ratio))
}

fn to_human_size(s: f64) -> String {
    if s < 1_043_333.12 {
        format!("{:.2} KiB", s / 1_024.0)
    } else if s < 1_068_373_114.88 {
        format!("{:.2} MiB", s / 1_048_576.0)
    } else if s < 1_094_014_069_637.12 {
        format!("{:.2} GiB", s / 1_073_741_824.0)
    } else {
        format!("{:.2} TiB", s / 1_099_511_627_776.0)
    }
}

/// Generates a shell script for invoking `dbgen` into stdout.
pub fn print_script(args: &Args) {
    let schema_name = QName::parse(&args.schema_name).expect("valid schema name");
    let quoted_schema_name = shlex::try_quote(&args.schema_name).expect("valid schema name");

    let meta_seed = args.seed.unwrap_or_else(|| OsRng.r#gen());
    println!(
        "#!/bin/sh\n\
         # generated by dbschemagen v{} ({}), using seed {}\n\n\
         set -eu\n\
         echo 'CREATE SCHEMA '{}';' > {}-schema-create.sql\n",
        env!("CARGO_PKG_VERSION"),
        env!("VERGEN_GIT_SHA").get(..9).unwrap_or("unofficial release"),
        meta_seed,
        quoted_schema_name,
        schema_name.unique_name(),
    );

    let exe_suffix = if cfg!(windows) { ".exe" } else { "" };

    let rng = meta_seed.make_rng();
    let extra_args = shlex::try_join(args.args.iter().map(|s| &**s)).expect("valid arguments");
    let rows_count_per_file = args.rows_count * args.inserts_count;
    for (i, table) in gen_tables(args.dialect, rng, args.size, args.tables_count).enumerate() {
        println!(
            "# table: s{}, rows count: {}, estimated size: {}\n\
             dbgen{} -i - -o . -s {} -t {}.s{} -R {} -r {} -N {} \
             {} <<SCHEMAEOF\n{}\nSCHEMAEOF\n",
            i,
            table.rows_count,
            to_human_size(table.target_size),
            exe_suffix,
            table.seed,
            quoted_schema_name,
            i,
            rows_count_per_file,
            args.rows_count,
            table.rows_count,
            extra_args,
            table.schema,
        );
    }
}
