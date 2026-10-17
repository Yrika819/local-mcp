p='src/goal_api.rs'
s=open(p,encoding='utf-8').read()
old='''    let request: GoalResultRequest = parse_request(args)?;
    validate_session_binding(&request.session_id, session)?;
    let goal_id = parse_goal_id(&request.goal_id)?;
    let store = store.clone();
    let session_id = session.id.clone();
    let goal_id_for_load = goal_id.clone();
    let goal = tokio::task::spawn_blocking(move || {
        store
            .load_goal(&session_id, &goal_id_for_load)
            .map_err(GoalApiError::from_orchestrator)
    })
    .await
    .map_err(|error| {
        GoalApiError::from_orchestrator(OrchestratorError::PersistenceIo(std::io::Error::other(
            error.to_string(),
        )))
    })??;
    Ok(result_view(&goal))
}'''
new='''    let request: GoalResultRequest = parse_request(args)?;
    validate_session_binding(&request.session_id, session)?;
    let goal_id = parse_goal_id(&request.goal_id)?;
    let goal = store
        .load_goal(&session.id, &goal_id)
        .map_err(GoalApiError::from_orchestrator)?;
    Ok(result_view(&goal))
}'''
assert old in s
s=s.replace(old,new,1)
open(p,'w',encoding='utf-8').write(s)
print("ok")