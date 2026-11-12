//! Which foreground scripts print file content rather than command output.

use aither_sandbox::{OutputTreatment, ScriptCheck, TreatOutput};

use crate::command_reading::{CommandReading, Intent};

/// Keeps the output of a script that only reads files verbatim, so the agent
/// reads the file with its blank lines; every other script's output is
/// cleaned as command output.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileReadsAreVerbatim;

impl TreatOutput for FileReadsAreVerbatim {
    fn treatment(&self, script: &ScriptCheck<'_>) -> OutputTreatment {
        // A script in another language, or one that is not valid shell, is
        // no file read: `CommandReading` reads shell only.
        if !matches!(script.shell, Some(Ok(_))) {
            return OutputTreatment::Command;
        }
        let reading = CommandReading::of(script.script);
        let only_reads = !reading.steps.is_empty()
            && reading
                .steps
                .iter()
                .all(|step| matches!(step.intent, Intent::Read { .. }));
        if only_reads {
            OutputTreatment::Verbatim
        } else {
            OutputTreatment::Command
        }
    }
}

#[cfg(test)]
mod tests;
