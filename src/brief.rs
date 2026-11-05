//! Reglas que el usuario fija antes de entrar al chat.
//!
//! Hay tres grupos: lo que no debe hacer, lo que sí o sí debe hacer
//! y lo que tiene que tener en cuenta durante la charla. Cada grupo
//! se elige con varios ítems y se puede completar escribiendo.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleGroup {
    Forbid,
    Require,
    Remember,
}

pub struct RulePreset {
    pub id: &'static str,
    pub group: RuleGroup,
    pub label: &'static str,
    pub instruction: &'static str,
}

const PRESETS: &[RulePreset] = &[
    RulePreset {
        id: "no-borrar",
        group: RuleGroup::Forbid,
        label: "Borrar archivos",
        instruction: "No borres archivos ni carpetas.",
    },
    RulePreset {
        id: "no-git",
        group: RuleGroup::Forbid,
        label: "Commit o push",
        instruction: "No hagas commit, push ni cambies el historial de Git.",
    },
    RulePreset {
        id: "no-deps",
        group: RuleGroup::Forbid,
        label: "Instalar dependencias",
        instruction: "No instales dependencias ni cambies el gestor de paquetes.",
    },
    RulePreset {
        id: "no-config",
        group: RuleGroup::Forbid,
        label: "Tocar configuración",
        instruction: "No modifiques configuración, secretos ni archivos de entorno.",
    },
    RulePreset {
        id: "no-extra",
        group: RuleGroup::Forbid,
        label: "Salirse del pedido",
        instruction: "No cambies archivos que no hagan falta para el pedido.",
    },
    RulePreset {
        id: "no-destructivo",
        group: RuleGroup::Forbid,
        label: "Comandos destructivos",
        instruction: "No ejecutes comandos destructivos (borrar en masa, reset --hard, formatear).",
    },
    RulePreset {
        id: "listar",
        group: RuleGroup::Require,
        label: "Listar los cambios",
        instruction: "Al terminar, listá cada archivo que cambiaste y por qué.",
    },
    RulePreset {
        id: "estilo",
        group: RuleGroup::Require,
        label: "Seguir el estilo",
        instruction: "Respetá el estilo, los nombres y las convenciones que ya usa el proyecto.",
    },
    RulePreset {
        id: "minimo",
        group: RuleGroup::Require,
        label: "Cambios mínimos",
        instruction: "Hacé el cambio más chico que resuelva el pedido.",
    },
    RulePreset {
        id: "pruebas",
        group: RuleGroup::Require,
        label: "Actualizar pruebas",
        instruction: "Si el repo tiene pruebas, agregá o actualizá las que correspondan a este cambio.",
    },
    RulePreset {
        id: "coherente",
        group: RuleGroup::Require,
        label: "Dejarlo coherente",
        instruction: "El código que dejes tiene que ser coherente con el stack del repo.",
    },
    RulePreset {
        id: "es",
        group: RuleGroup::Remember,
        label: "Responder en español",
        instruction: "Respondé siempre en español.",
    },
    RulePreset {
        id: "rutas",
        group: RuleGroup::Remember,
        label: "Citar rutas reales",
        instruction: "Citá rutas reales del repositorio.",
    },
    RulePreset {
        id: "preguntar",
        group: RuleGroup::Remember,
        label: "Preguntar si es grande",
        instruction: "Si el cambio es grande o ambiguo, preguntá antes de seguir.",
    },
    RulePreset {
        id: "reusar",
        group: RuleGroup::Remember,
        label: "Reusar lo que existe",
        instruction: "Reutilizá lo que ya está en el proyecto antes de crear algo nuevo.",
    },
    RulePreset {
        id: "narrar",
        group: RuleGroup::Remember,
        label: "Decir qué hace",
        instruction: "Mientras trabajás, decí en una frase qué estás haciendo.",
    },
];

pub fn presets(group: RuleGroup) -> impl Iterator<Item = &'static RulePreset> {
    PRESETS.iter().filter(move |preset| preset.group == group)
}

/// Reglas vivas de un chat. Viajan en cada mensaje hasta que el usuario las cambia.
#[derive(Clone, Debug)]
pub struct ChatBrief {
    pub confirmed: bool,
    /// Ya se inicializó el interruptor de código según el modo elegido.
    pub seeded: bool,
    pub write_code: bool,
    pub selected: Vec<String>,
    pub forbid_note: String,
    pub require_note: String,
    pub remember_note: String,
}

impl ChatBrief {
    pub fn fresh() -> Self {
        Self {
            confirmed: false,
            seeded: false,
            write_code: false,
            selected: vec![
                "es".into(),
                "listar".into(),
                "minimo".into(),
                "rutas".into(),
            ],
            forbid_note: String::new(),
            require_note: String::new(),
            remember_note: String::new(),
        }
    }

    pub fn is_on(&self, id: &str) -> bool {
        self.selected.iter().any(|item| item == id)
    }

    pub fn toggle(&mut self, id: &str) {
        if let Some(index) = self.selected.iter().position(|item| item == id) {
            self.selected.remove(index);
        } else {
            self.selected.push(id.to_string());
        }
    }

    pub fn summary_line(&self) -> String {
        let rules = self.selected.len()
            + usize::from(!self.forbid_note.trim().is_empty())
            + usize::from(!self.require_note.trim().is_empty())
            + usize::from(!self.remember_note.trim().is_empty());
        let code = if self.write_code {
            "Puede escribir código"
        } else {
            "Sin tocar archivos"
        };
        format!("{code} · {rules} reglas")
    }

    /// Bloque que se antepone al prompt. Vacío si no hay nada que decir.
    pub fn decorate(&self, prompt: &str, plan_only: bool) -> String {
        let block = self.prompt_block(plan_only);
        if block.trim().is_empty() {
            prompt.to_string()
        } else {
            format!("{block}\n\n---\n\n{prompt}")
        }
    }

    fn prompt_block(&self, plan_only: bool) -> String {
        let mut out = String::new();
        out.push_str("# Reglas de esta conversación\n\n");
        out.push_str(
            "Seguí estas reglas en cada respuesta. Tienen prioridad sobre hacer de más.\n",
        );

        let mut wrote_section = false;
        wrote_section |= push_group(
            &mut out,
            "No hagas",
            RuleGroup::Forbid,
            &self.selected,
            &self.forbid_note,
        );
        wrote_section |= push_group(
            &mut out,
            "Hacé siempre",
            RuleGroup::Require,
            &self.selected,
            &self.require_note,
        );
        wrote_section |= push_group(
            &mut out,
            "Tené en cuenta durante la charla",
            RuleGroup::Remember,
            &self.selected,
            &self.remember_note,
        );

        out.push_str("\n## Código\n");
        if plan_only {
            out.push_str(
                "En este turno no implementes código: sólo el plan pedido. No uses bloques `file:`.\n",
            );
        } else if self.write_code {
            out.push_str(
                "Cuando el pedido implique cambiar el proyecto, escribí los archivos.\n\
                 Cada archivo va completo, con esta marca y la ruta relativa al repo:\n\n\
                 ```file:src/ejemplo.rs\n\
                 contenido completo del archivo\n\
                 ```\n\n\
                 No uses esa marca para ejemplos ni para fragmentos. Después de los bloques, \
                 resumí qué cambió y por qué. Si no hace falta tocar archivos, no inventes bloques.\n",
            );
        } else {
            out.push_str(
                "No modifiques archivos del proyecto. Respondé con la explicación, sin aplicar cambios.\n",
            );
        }
        let _ = wrote_section;
        out
    }
}

fn push_group(
    out: &mut String,
    title: &str,
    group: RuleGroup,
    selected: &[String],
    note: &str,
) -> bool {
    let instructions: Vec<&str> = presets(group)
        .filter(|preset| selected.iter().any(|id| id == preset.id))
        .map(|preset| preset.instruction)
        .collect();
    let note = note.trim();
    if instructions.is_empty() && note.is_empty() {
        return false;
    }
    out.push_str(&format!("\n## {title}\n"));
    for line in instructions {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
    if !note.is_empty() {
        out.push_str("- Nota del usuario: ");
        out.push_str(note);
        out.push('\n');
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_prompt_incluye_lo_elegido_y_lo_escrito() {
        let mut brief = ChatBrief::fresh();
        brief.write_code = true;
        brief.toggle("no-borrar");
        brief.remember_note = "No uses abreviaturas.".into();
        let prompt = brief.decorate("pregunta", false);
        assert!(prompt.contains("No borres archivos"));
        assert!(prompt.contains("Respondé siempre en español"));
        assert!(prompt.contains("No uses abreviaturas."));
        assert!(prompt.contains("```file:src/ejemplo.rs"));
        assert!(prompt.contains("pregunta"));
    }

    #[test]
    fn un_plan_no_pide_escribir_archivos() {
        let mut brief = ChatBrief::fresh();
        brief.write_code = true;
        let prompt = brief.decorate("armar plan", true);
        assert!(prompt.contains("no implementes código"));
        assert!(!prompt.contains("```file:src/ejemplo.rs"));
    }

    #[test]
    fn el_interruptor_de_reglas_agrega_y_saca() {
        let mut brief = ChatBrief::fresh();
        assert!(brief.is_on("es"));
        brief.toggle("es");
        assert!(!brief.is_on("es"));
        brief.toggle("es");
        assert!(brief.is_on("es"));
    }
}
