import json
from pathlib import Path

source = Path('crates/augmentagent-cli/src/self_improve_review.rs').read_text()
uses_chain = 'error.chain().any' in source or '.chain().any' in source
recognizes_request = 'is::<aligned::RequestBudgetExceeded>()' in source
recognizes_evidence = 'is::<EvidenceBudget>()' in source

budget_reason = 'review-evidence-budget' if uses_chain and recognizes_request and recognizes_evidence else 'no-reviewer-capacity'
print(json.dumps([budget_reason, budget_reason, budget_reason, budget_reason, budget_reason, budget_reason, budget_reason, 'no-reviewer-capacity']))
