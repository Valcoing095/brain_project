//! Planes de trabajo parseados desde markdown del asistente.

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct PlanStep {
    pub title: String,
    pub detail: String,
    pub done: bool,
}

#[derive(Clone, Debug)]
pub struct WorkPlan {
    pub id: String,
    pub title: String,
    pub goal: String,
    pub steps: Vec<PlanStep>,
    pub raw_markdown: String,
    pub created_unix: u64,
}

impl WorkPlan {
    pub fn from_markdown(goal_hint: &str, markdown: &str) -> Self {
        let created_unix = now_unix();
        let id = format!("plan-{created_unix}");
        let title = extract_title(markdown).unwrap_or_else(|| {
            let hint = goal_hint.trim();
            if hint.is_empty() {
                "Plan de trabajo".into()
            } else {
                truncate_chars(hint, 48)
            }
        });
        let goal = extract_goal(markdown).unwrap_or_else(|| goal_hint.trim().to_string());
        let steps = extract_steps(markdown);
        Self {
            id,
            title,
            goal,
            steps,
            raw_markdown: markdown.trim().to_string(),
            created_unix,
        }
    }

    pub fn develop_prompt(&self) -> String {
        let mut s = String::new();
        s.push_str("Desarrollá este plan de trabajo sobre el proyecto actual.\n");
        s.push_str("Seguí los pasos en orden. Tocá sólo lo necesario. Citá rutas reales.\n\n");
        s.push_str(&format!("# {}\n\n", self.title));
        if !self.goal.trim().is_empty() {
            s.push_str(&format!("## Objetivo\n{}\n\n", self.goal.trim()));
        }
        s.push_str("## Pasos\n");
        for (index, step) in self.steps.iter().enumerate() {
            s.push_str(&format!("{}. {}\n", index + 1, step.title));
            if !step.detail.trim().is_empty() {
                s.push_str(&format!("   {}\n", step.detail.trim()));
            }
        }
        if self.steps.is_empty() {
            s.push_str("\n");
            s.push_str(&self.raw_markdown);
        }
        s
    }
}

/// Instrucciones que se anteponen al pedido del usuario para forzar un plan estructurado.
pub fn plan_request_prompt(user_goal: &str) -> String {
    format!(
        "Generá un PLAN DE TRABAJO para este pedido. No implementes código todavía.\n\n\
         Pedido del usuario:\n{user_goal}\n\n\
         Respondé SOLO en markdown con esta forma:\n\
         # Título corto del plan\n\
         ## Objetivo\n\
         (1-3 oraciones)\n\
         ## Pasos\n\
         1. Paso concreto y accionable\n\
         2. Otro paso\n\
         3. …\n\
         ## Notas\n\
         (riesgos, archivos clave, criterios de hecho)\n\n\
         Cada paso debe ser verificable. Usá rutas o módulos del repo cuando sepas cuáles son."
    )
}

fn extract_title(markdown: &str) -> Option<String> {
    for line in markdown.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let title = rest.trim();
            if !title.is_empty() && !title.eq_ignore_ascii_case("plan de trabajo") {
                return Some(truncate_chars(title, 72));
            }
        }
    }
    None
}

fn extract_goal(markdown: &str) -> Option<String> {
    let mut in_goal = false;
    let mut lines = Vec::new();
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("## ") {
            let heading = trimmed.trim_start_matches('#').trim().to_ascii_lowercase();
            if in_goal {
                break;
            }
            in_goal = heading.contains("objetivo") || heading.contains("meta") || heading.contains("goal");
            continue;
        }
        if in_goal {
            if trimmed.is_empty() && !lines.is_empty() {
                break;
            }
            if !trimmed.is_empty() {
                lines.push(trimmed.to_string());
            }
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join(" "))
    }
}

fn extract_steps(markdown: &str) -> Vec<PlanStep> {
    let mut steps = Vec::new();
    let mut in_steps = false;
    let mut pending: Option<PlanStep> = None;

    let flush = |pending: &mut Option<PlanStep>, steps: &mut Vec<PlanStep>| {
        if let Some(step) = pending.take() {
            if !step.title.trim().is_empty() {
                steps.push(step);
            }
        }
    };

    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#' ) {
            let heading = trimmed.trim_start_matches('#').trim().to_ascii_lowercase();
            if heading.contains("paso") || heading.contains("step") || heading == "pasos" {
                in_steps = true;
                continue;
            }
            if in_steps
                && (heading.contains("nota")
                    || heading.contains("riesgo")
                    || heading.contains("criterio")
                    || heading.contains("objetivo"))
            {
                flush(&mut pending, &mut steps);
                in_steps = false;
            }
            continue;
        }

        if let Some(title) = numbered_item(trimmed).or_else(|| bullet_item(trimmed)) {
            flush(&mut pending, &mut steps);
            in_steps = true;
            pending = Some(PlanStep {
                title,
                detail: String::new(),
                done: false,
            });
            continue;
        }

        if in_steps {
            if let Some(step) = pending.as_mut() {
                if !trimmed.is_empty() {
                    if !step.detail.is_empty() {
                        step.detail.push(' ');
                    }
                    step.detail.push_str(trimmed);
                }
            }
        }
    }
    flush(&mut pending, &mut steps);

    if steps.is_empty() {
        // Fallback: cada ## distinto de objetivo/notas como paso.
        for line in markdown.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("## ") {
                let lower = rest.to_ascii_lowercase();
                if lower.contains("objetivo")
                    || lower.contains("meta")
                    || lower.contains("nota")
                    || lower.contains("riesgo")
                {
                    continue;
                }
                steps.push(PlanStep {
                    title: rest.trim().to_string(),
                    detail: String::new(),
                    done: false,
                });
            }
        }
    }
    steps
}

fn numbered_item(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let rest = line[i..].trim_start();
    let rest = rest
        .strip_prefix('.')
        .or_else(|| rest.strip_prefix(')'))
        .or_else(|| rest.strip_prefix('-'))?;
    let title = rest.trim();
    (!title.is_empty()).then(|| title.to_string())
}

fn bullet_item(line: &str) -> Option<String> {
    for prefix in ["- ", "* ", "• "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            let title = rest.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsea_titulo_objetivo_y_pasos_numerados() {
        let md = "# Login con OAuth\n\n## Objetivo\nPermitir entrar con Google.\n\n## Pasos\n1. Agregar dependencia oauth\n2. Crear ruta /auth/callback\n   Validar el state.\n3. Guardar sesión\n\n## Notas\nUsar HTTPS.\n";
        let plan = WorkPlan::from_markdown("oauth", md);
        assert_eq!(plan.title, "Login con OAuth");
        assert!(plan.goal.contains("Google"));
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].title, "Agregar dependencia oauth");
        assert!(plan.steps[1].detail.contains("Validar"));
        assert!(plan.develop_prompt().contains("Desarrollá este plan"));
    }
}
