// Which control was acted on is the only proof that a pick was honoured, and
// the harness cannot read a click's target after the fact. So the page reports.
function tell() {
  const id = event.currentTarget.id;
  fetch('/arbiter/__click', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ id }),
  });
  document.title = 'clicked ' + id;
}
