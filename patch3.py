p='src/goal_api.rs'
s=open(p,encoding='utf-8').read()
old='''    .await
    .map_err(|error| {
        GoalApiError::from_orchestrator(OrchestratorError::PersistenceIo(std::io::Error::other(
            error.to_string(),
        )))
    })?;'''
new='''    .await
    .map_err(|error| {
        GoalApiError::from_orchestrator(OrchestratorError::PersistenceIo(std::io::Error::other(
            error.to_string(),
        )))
    })??;'''
assert old in s
s=s.replace(old,new)
open(p,'w',encoding='utf-8').write(s)
print("ok")