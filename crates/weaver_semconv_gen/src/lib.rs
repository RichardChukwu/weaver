// SPDX-License-Identifier: Apache-2.0

//! This crate will generate code for markdown files.
//! The entire crate is a rush job to catch feature parity w/ existing python tooling by
//! poorly porting the code into RUST.  We expect to optimise and improve things over time.

#![deny(
    missing_docs,
    clippy::print_stdout,
    unstable_features,
    unused_import_braces,
    unused_qualifications,
    unused_results,
    unused_extern_crates
)]

use std::fs;
use weaver_logger::Logger;
use weaver_resolved_schema::attribute::{Attribute, AttributeRef};
use weaver_resolved_schema::registry::{Group, Registry};
use weaver_resolved_schema::ResolvedTelemetrySchema;
use weaver_resolver::SchemaResolver;
use weaver_semconv::SemConvRegistry;

mod diff;
mod gen;
mod parser;

use crate::gen::{AttributeTableView, GenerateMarkdownContext, MetricView};

/// Errors emitted by this crate.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Thrown when we are unable to find a semconv by id.
    #[error("Could not find: {id}")]
    GroupNotFound {
        /// The id of the semconv lookup
        id: String,
    },

    /// Thrown when forcing a group to be a metricl
    #[error("Expected metric: {id}")]
    GroupMustBeMetric {
        /// The id of the semconv lookup that was not a metric.
        id: String,
    },
    /// Thrown when rendering an attribute group, but no attributes remain after filtering.
    #[error("No attributes retained for '{id}' filtering by '{filter}'")]
    GroupHasNoRenderableAttributesAfterFilter {
        /// The id of the semconv lookup.
        id: String,
        /// The filter for which attributes to display.
        filter: String,
    },
    /// Errors thrown when we are running a dry run and markdown doesn't match.
    #[error("Markdown is not equal:\n{}", diff::diff_output(.original, .updated))]
    MarkdownIsNotEqual {
        /// Original markdown value.
        original: String,
        /// Updated markdown value.
        updated: String,
        // TODO - smart diff.
    },
    /// Thrown when snippet header is invalid.
    #[error("Could not parse snippet header: [{header}]")]
    InvalidSnippetHeader {
        /// Markdown snippet identifer <!-- semconv {header} -->
        header: String,
    },
    /// Errors from using std io library.
    #[error(transparent)]
    StdIoError(#[from] std::io::Error),

    /// Errors from using std fmt library.
    #[error(transparent)]
    StdFmtError(#[from] std::fmt::Error),

    /// Errors from using weaver_semconv.
    #[error(transparent)]
    SemconvError(#[from] weaver_semconv::Error),

    /// Errors from using weaver_resolver.
    #[error(transparent)]
    ResolverError(#[from] weaver_resolver::Error),
}

// TODO - this is based on https://github.com/open-telemetry/build-tools/blob/main/semantic-conventions/src/opentelemetry/semconv/templating/markdown/__init__.py#L503
// We can likely model this much better.
/// Parameters users can specify for generating markdown.
#[derive(Clone, Debug, PartialEq)]
pub enum MarkdownGenParameters {
    /// Filter attributes to those with a given tag.
    Tag(String),
    /// Display all metrics in a group?
    Full,
    /// Generate a metric table
    MetricTable,
    /// Omit the requirement level.
    OmitRequirementLevel,
}
/// Markdown-snippet generation arguments.
pub struct GenerateMarkdownArgs {
    /// The id of the metric, event, span or attribute group to render.
    id: String,
    /// Arguments the user specified that we've parsed.
    args: Vec<MarkdownGenParameters>,
}
impl GenerateMarkdownArgs {
    // TODO
    // fn is_full(&self) -> bool {
    //     self.args.iter().any(|a| matches!(a, MarkdownGenParameters::Full))
    // }
    /// Returns true if the omit requirement level flag was specified.
    fn is_omit_requirement(&self) -> bool {
        self.args
            .iter()
            .any(|a| matches!(a, MarkdownGenParameters::OmitRequirementLevel))
    }
    /// Returns true if a metric table should be rendered.
    fn is_metric_table(&self) -> bool {
        self.args
            .iter()
            .any(|a| matches!(a, MarkdownGenParameters::MetricTable))
    }
}

/// Constructs a markdown snippet (without header/closer)
fn generate_markdown_snippet(
    lookup: &ResolvedSemconvRegistry,
    args: GenerateMarkdownArgs,
) -> Result<String, Error> {
    let mut ctx = GenerateMarkdownContext::default();
    let mut result = String::new();
    if args.is_metric_table() {
        let view = MetricView::try_new(args.id.as_str(), lookup)?;
        view.generate_markdown(&mut result, &mut ctx)?;
    } else {
        let other = AttributeTableView::try_new(args.id.as_str(), lookup)?;
        other.generate_markdown(&mut result, &args, &mut ctx)?;
    }
    Ok(result)
}

// TODO - This entire function could be optimised and reworked.
fn update_markdown_contents(
    contents: &str,
    lookup: &ResolvedSemconvRegistry,
) -> Result<String, Error> {
    let mut result = String::new();
    let mut handling_snippet = false;
    for line in contents.lines() {
        if handling_snippet {
            if parser::is_semconv_trailer(line) {
                result.push_str(line);
                // TODO - do we always need this or did we trim oddly?
                result.push('\n');
                handling_snippet = false;
            }
        } else {
            // Always push this line.
            result.push_str(line);
            // TODO - don't do this on last line.
            result.push('\n');
            // Check to see if line matches snippet request.
            // If so, generate the snippet and continue.
            if parser::is_markdown_snippet_directive(line) {
                handling_snippet = true;
                let arg = parser::parse_markdown_snippet_directive(line)?;
                let snippet = generate_markdown_snippet(lookup, arg)?;
                result.push_str(&snippet);
            }
        }
    }
    Ok(result)
}

/// Updates a single markdown file using the resolved schema.
pub fn update_markdown(
    file: &str,
    lookup: &ResolvedSemconvRegistry,
    dry_run: bool,
) -> Result<(), Error> {
    let original_markdown = fs::read_to_string(file)?;
    let updated_markdown = update_markdown_contents(&original_markdown, lookup)?;
    if !dry_run {
        fs::write(file, updated_markdown)?;
        Ok(())
    } else if original_markdown != updated_markdown {
        Err(Error::MarkdownIsNotEqual {
            original: original_markdown,
            updated: updated_markdown,
        })
    } else {
        Ok(())
    }
}

/// The resolved Semantic Convention repository that is used to drive snipper generation.
pub struct ResolvedSemconvRegistry {
    schema: ResolvedTelemetrySchema,
    registry_id: String,
}
impl ResolvedSemconvRegistry {
    /// Resolve the semantic convention registry and make it available for rendering markdown snippets.
    pub fn try_from_path(
        path_pattern: &str,
        log: impl Logger + Clone + Sync,
    ) -> Result<ResolvedSemconvRegistry, Error> {
        let registry_id = "semantic_conventions";
        let mut registry = SemConvRegistry::try_from_path(registry_id, path_pattern)?;
        let schema = SchemaResolver::resolve_semantic_convention_registry(&mut registry, log)?;
        let lookup = ResolvedSemconvRegistry {
            schema,
            registry_id: registry_id.into(),
        };
        Ok(lookup)
    }

    fn my_registry(&self) -> Option<&Registry> {
        self.schema.registry(self.registry_id.as_str())
    }

    fn find_group(&self, id: &str) -> Option<&Group> {
        self.my_registry()
            .and_then(|r| r.groups.iter().find(|g| g.id == id))
    }

    /// Finds an attribute by reference.
    fn attribute(&self, attr: &AttributeRef) -> Option<&Attribute> {
        self.schema.catalog.attribute(attr)
    }
}

#[cfg(test)]
mod tests {
    use weaver_logger::TestLogger;

    use crate::{update_markdown, Error, ResolvedSemconvRegistry};

    fn force_print_error<T>(result: Result<T, Error>) -> T {
        match result {
            Err(err) => panic!("{}", err),
            Ok(v) => v,
        }
    }

    #[test]
    fn test_http_semconv() -> Result<(), Error> {
        let logger = TestLogger::default();
        let lookup = ResolvedSemconvRegistry::try_from_path("data/**/*.yaml", logger.clone())?;

        // Check our test files.
        force_print_error(update_markdown(
            "data/http-span-full-attribute-table.md",
            &lookup,
            true,
        ));
        force_print_error(update_markdown(
            "data/http-metric-semconv.md",
            &lookup,
            true,
        ));
        Ok(())
    }
}
