//! The typed count (UX-T35) and the proof that it was typed for exactly this plan (Task 6.6.3a, ADR-006 A6,
//! review C6 and review 2 N5).
//!
//! The chain is linear: a [`ReviewedPlan`] goes into [`Confirm::new`], [`Confirm::accepts`] consumes the
//! `Confirm` and returns a [`ConfirmedPlan`] that contains the reviewed plan, and the apply takes only a
//! `ConfirmedPlan`. A typed count therefore cannot be replayed against another plan, reused after the delete,
//! or paired with loose fields.
//!
//! The typed count is a speed bump against a reflex, not a defence against another program of the same uid:
//! such a program can write the same bytes to the same terminal (C11, ADR-006 A6). The real reader of the
//! entry is a port ([`super::ports::TypedCountSource`]) implemented in Story 6.6.4c.

use std::path::{Path, PathBuf};

use crate::explorer_plan::ExplorerPlan;
use crate::plan::{ItemIdentity, PlanId, Planned};
use crate::roots::RootIdentity;

/// One item of a reviewed plan: the path and what `lstat` said it was when the plan was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewedItem {
    pub path: PathBuf,
    pub identity: ItemIdentity,
}

/// The plan the owner reviewed: every item with its planned identity, each scan root with its identity and the
/// plan-id derived from both (never stored separately, so they cannot disagree). No `Clone`, `Copy` or
/// `Default`: the session holds one, and a delete consumes it.
#[derive(Debug, PartialEq, Eq)]
pub struct ReviewedPlan {
    plan_id: PlanId,
    items: Vec<ReviewedItem>,
    roots: Vec<RootIdentity>,
    bytes: u64,
}

impl ReviewedPlan {
    /// `bytes` is the attributed total the owner saw.
    pub fn new(items: Vec<ReviewedItem>, roots: Vec<RootIdentity>, bytes: u64) -> ReviewedPlan {
        let plan_id =
            PlanId::for_explorer(items.iter().map(|i| (i.path.as_path(), i.identity)), &roots);
        ReviewedPlan {
            plan_id,
            items,
            roots,
            bytes,
        }
    }

    /// The items that would be removed (`Planned::Will` with an identity), in the plan's order; the plan-id
    /// equals [`ExplorerPlan::plan_id`].
    pub fn from_explorer_plan(plan: &ExplorerPlan) -> ReviewedPlan {
        let items = plan
            .rows
            .iter()
            .filter(|r| r.planned == Planned::Will)
            .filter_map(|r| {
                r.identity.map(|identity| ReviewedItem {
                    path: r.path.clone(),
                    identity,
                })
            })
            .collect();
        ReviewedPlan::new(items, plan.roots.clone(), plan.total_bytes())
    }

    pub fn plan_id(&self) -> PlanId {
        self.plan_id
    }

    pub fn count(&self) -> usize {
        self.items.len()
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn items(&self) -> &[ReviewedItem] {
        &self.items
    }

    pub fn roots(&self) -> &[RootIdentity] {
        &self.roots
    }

    pub fn item(&self, path: &Path) -> Option<&ReviewedItem> {
        self.items.iter().find(|i| i.path == path)
    }
}

/// Why the entry was not accepted. Hands the [`Confirm`] back so the prompt stays open.
#[derive(Debug)]
pub struct ConfirmRefusal {
    text: String,
    confirm: Confirm,
}

impl ConfirmRefusal {
    /// `That is not 3. Nothing was deleted.`
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The same prompt, for the next entry.
    pub fn into_confirm(self) -> Confirm {
        self.confirm
    }
}

impl std::fmt::Display for ConfirmRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// The open prompt for one reviewed plan.
#[derive(Debug)]
pub struct Confirm {
    reviewed: ReviewedPlan,
}

impl Confirm {
    /// Consumes the plan: after this the only way back to a deletable value is [`Confirm::accepts`].
    pub fn new(reviewed: ReviewedPlan) -> Confirm {
        Confirm { reviewed }
    }

    pub fn count(&self) -> usize {
        self.reviewed.count()
    }

    pub fn bytes(&self) -> u64 {
        self.reviewed.bytes()
    }

    /// The plan this prompt is for, to show its items.
    pub fn reviewed(&self) -> &ReviewedPlan {
        &self.reviewed
    }

    /// Esc: closes the prompt and hands the plan back, so a cancelled prompt keeps the review. The plan can
    /// only become deletable again through [`Confirm::new`] and [`Confirm::accepts`].
    pub fn into_reviewed(self) -> ReviewedPlan {
        self.reviewed
    }

    /// `Type 3 and press Enter to delete, Esc to cancel:`
    pub fn prompt_line(&self) -> String {
        format!(
            "Type {} and press Enter to delete, Esc to cancel:",
            self.count()
        )
    }

    /// True only for the exact decimal digits of `count`: no sign, no padding, no whitespace, no word.
    /// Pure and free of the plan so a mutation run can pin each arm.
    pub fn digits_match(count: usize, input: &str) -> bool {
        input == count.to_string()
    }

    /// Accepts exactly the decimal digits of the count. Consumes the prompt; a wrong entry gives it back
    /// inside the refusal. An empty plan is never accepted.
    pub fn accepts(self, input: &str) -> Result<ConfirmedPlan, ConfirmRefusal> {
        let count = self.reviewed.count();
        if count > 0 && Confirm::digits_match(count, input) {
            return Ok(ConfirmedPlan {
                reviewed: self.reviewed,
                typed: input.to_string(),
            });
        }
        let text = if count == 0 {
            "There is nothing to delete. Nothing was deleted.".to_string()
        } else {
            format!("That is not {count}. Nothing was deleted.")
        };
        Err(ConfirmRefusal {
            text,
            confirm: self,
        })
    }
}

/// The audit data of an `explorer_confirm` line (Task 6.6.3c writes it): derived from a [`ConfirmedPlan`],
/// never an input of the apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmRecord {
    pub plan_id: PlanId,
    pub count: usize,
    pub bytes: u64,
    pub typed: String,
}

/// A reviewed plan plus the count that was typed for it. Fields are private and [`Confirm::accepts`] is the
/// only constructor; there is no `Clone`, `Copy`, `Default` or `Deserialize`, so one confirmation authorises
/// one delete of this plan and nothing else.
///
/// ```compile_fail,E0451
/// use dunnage_core::explore::confirm::{ConfirmedPlan, ReviewedPlan};
/// fn forge(reviewed: ReviewedPlan) -> ConfirmedPlan {
///     ConfirmedPlan { reviewed, typed: "3".into() }
/// }
/// ```
///
/// ```compile_fail,E0599
/// use dunnage_core::explore::confirm::ConfirmedPlan;
/// let _ = ConfirmedPlan::default();
/// ```
///
/// ```compile_fail,E0599
/// use dunnage_core::explore::confirm::ConfirmedPlan;
/// fn twice(c: ConfirmedPlan) -> (ConfirmedPlan, ConfirmedPlan) { (c.clone(), c) }
/// ```
///
/// ```compile_fail,E0382
/// use dunnage_core::explore::confirm::ConfirmedPlan;
/// fn use_after_move(c: ConfirmedPlan) { drop(c); let _ = c.count(); }
/// ```
///
/// ```compile_fail,E0277
/// use dunnage_core::explore::confirm::ConfirmedPlan;
/// fn copy<T: Copy>() {}
/// fn f() { copy::<ConfirmedPlan>(); }
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct ConfirmedPlan {
    reviewed: ReviewedPlan,
    typed: String,
}

impl ConfirmedPlan {
    pub fn plan_id(&self) -> PlanId {
        self.reviewed.plan_id()
    }

    pub fn count(&self) -> usize {
        self.reviewed.count()
    }

    pub fn bytes(&self) -> u64 {
        self.reviewed.bytes()
    }

    pub fn typed(&self) -> &str {
        &self.typed
    }

    pub fn reviewed(&self) -> &ReviewedPlan {
        &self.reviewed
    }

    pub fn record(&self) -> ConfirmRecord {
        ConfirmRecord {
            plan_id: self.plan_id(),
            count: self.count(),
            bytes: self.bytes(),
            typed: self.typed.clone(),
        }
    }

    /// The apply consumes the confirmation here and keeps the typed text for the audit line.
    pub fn into_parts(self) -> (ReviewedPlan, String) {
        (self.reviewed, self.typed)
    }
}
