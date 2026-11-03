#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentMode {
    Ask,
    Agent,
    Plan,
}

impl AgentMode {
    pub const ALL: [AgentMode; 3] = [AgentMode::Ask, AgentMode::Agent, AgentMode::Plan];

    pub fn title(self) -> &'static str {
        match self {
            Self::Ask => "Preguntar",
            Self::Agent => "Agente",
            Self::Plan => "Planificar",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Ask => "Conversación y dudas, sin tocar archivos.",
            Self::Agent => "Puede leer y modificar el proyecto.",
            Self::Plan => "Analiza y propone un plan, sin cambios.",
        }
    }

    pub fn cli_flag(self) -> Option<&'static str> {
        match self {
            Self::Ask => Some("ask"),
            Self::Plan => Some("plan"),
            Self::Agent => None,
        }
    }
}
