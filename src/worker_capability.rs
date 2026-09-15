use crate::task::WorkerKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadyWorkerRoute {
    Readonly,
    Writer,
}

const PRODUCTION_PLANNABLE: [WorkerKind; 2] = [WorkerKind::CodexReadonly, WorkerKind::CodexWriter];

pub(crate) fn production_plannable_worker_kinds() -> &'static [WorkerKind] {
    &PRODUCTION_PLANNABLE
}

pub(crate) fn is_production_plannable(worker: WorkerKind) -> bool {
    ready_worker_route(worker).is_some()
}

pub(crate) fn ready_worker_route(worker: WorkerKind) -> Option<ReadyWorkerRoute> {
    match worker {
        WorkerKind::CodexReadonly => Some(ReadyWorkerRoute::Readonly),
        WorkerKind::CodexWriter => Some(ReadyWorkerRoute::Writer),
        WorkerKind::LocalOperation | WorkerKind::CodexReviewer | WorkerKind::Verifier => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_plannable_workers_exactly_match_ready_routes() {
        let all = [
            WorkerKind::LocalOperation,
            WorkerKind::CodexReadonly,
            WorkerKind::CodexWriter,
            WorkerKind::CodexReviewer,
            WorkerKind::Verifier,
        ];
        let routed = all
            .into_iter()
            .filter(|worker| ready_worker_route(*worker).is_some())
            .collect::<Vec<_>>();
        assert_eq!(routed, production_plannable_worker_kinds());
        assert_eq!(
            ready_worker_route(WorkerKind::CodexReadonly),
            Some(ReadyWorkerRoute::Readonly)
        );
        assert_eq!(
            ready_worker_route(WorkerKind::CodexWriter),
            Some(ReadyWorkerRoute::Writer)
        );
        assert_eq!(ready_worker_route(WorkerKind::CodexReviewer), None);
        assert_eq!(ready_worker_route(WorkerKind::Verifier), None);
        assert_eq!(ready_worker_route(WorkerKind::LocalOperation), None);
    }
}
