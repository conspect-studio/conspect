# Final runtime gates — active integration

- Main caller scope approved in SOURCE_EVENT_MAIN_SCOPE_APPROVAL.md. Do not wait for another approval.
- Pipeline currently identifies TIMED_ID3 as metadata_streams false, but capture condition only Some(true) SCTE. Full issue requires actual ID3 metadata capture or explicit unsupported status; inventory alone is not implementation.
- HLS observer repair now gate-owned observer + sink retire completion fence, incomplete delta/headerempty/dangling EXTINF reject, adversarial barrier tests. Its native verification currently blocked by active pipeline syntax errors (7449/7466/7484 missing commas) and missing source_event_runtime module export. Finish compiling before final scoped tests.
- Core, API, UI, service, frame metadata are independently tested, but no completed runtime path/server proof yet. Follow progress ledger; do not report installed.
- Startup/live source add/remove must use pipeline-owned event authority and lifecycle fencing. NativeEvents Drop must producer.retire; held old frame context never receives latest mapping.
- Source events UI API schema finalized and generated, lint/build11tests pass. Runtime provider must publish exact selected-frame timer, availability, source identity and latest degradation scalars.
