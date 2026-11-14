//! Resource-backed slash commands shared by terminal and RPC discovery.

use rpi_harness::types::AgentHarnessResources;

#[derive(Default)]
pub(crate) struct ResourceCommandEntries {
    pub templates: Vec<(String, String)>,
    pub skills: Vec<(String, String)>,
}

/// Enumerate the current resource snapshot without presentation settings.
/// `disable_model_invocation` limits model selection, not explicit skill use.
pub(crate) fn resource_command_entries(
    resources: &AgentHarnessResources,
) -> ResourceCommandEntries {
    ResourceCommandEntries {
        templates: resources
            .prompt_templates
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|template| {
                (
                    template.name.clone(),
                    template
                        .description
                        .clone()
                        .unwrap_or_else(|| "Expand prompt template".into()),
                )
            })
            .collect(),
        skills: resources
            .skills
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|skill| (format!("skill:{}", skill.name), skill.description.clone()))
            .collect(),
    }
}
