//! Retained OXC programs shared by every script-processing pass.

use oxc_ast::ast as oxc;

use crate::ast::oxc_program::RetainedProgram;
use crate::ast::template::Root;

pub struct ParsedScript<'source> {
    retained: RetainedProgram<'source>,
}

impl<'source> ParsedScript<'source> {
    fn new(script: &mut crate::ast::template::Script<'source>) -> Self {
        let mut retained = RetainedProgram::parse(script.raw_content, true);
        // raw_content doubles as the lenient script-parse failure marker, and it
        // keeps that meaning after a recovery re-parse: the source really did not
        // parse, the transforms simply have a program to work from now.
        let parsed_cleanly = retained.diagnostics().is_empty();
        if retained.panicked()
            && let Some(recovered) = super::recovery::recover(script.raw_content, &retained)
        {
            retained = recovered;
        }
        if parsed_cleanly {
            script.raw_content = "";
        }
        Self { retained }
    }

    pub(crate) fn program(&self) -> &oxc::Program<'_> {
        self.retained.program()
    }

    pub(crate) fn source(&self) -> &str {
        self.retained.source()
    }
}

pub struct ParsedScripts<'source> {
    pub(crate) instance: Option<ParsedScript<'source>>,
    pub(crate) module: Option<ParsedScript<'source>>,
}

impl<'source> ParsedScripts<'source> {
    pub(crate) fn new(ast: &mut Root<'source>) -> Self {
        Self {
            instance: ast
                .instance
                .as_mut()
                .map(|script| ParsedScript::new(script)),
            module: ast.module.as_mut().map(|script| ParsedScript::new(script)),
        }
    }
}

#[inline]
pub(super) fn with_parsed_script<F, R>(script: &ParsedScript<'_>, f: F) -> R
where
    F: FnOnce(&oxc::Program, &str) -> R,
{
    f(script.program(), script.source())
}
