p='src/task_store.rs'
s=open(p,encoding='utf-8').read()
old='''pub(crate) struct TaskStore {
    state_root: PathBuf,
    #[cfg(test)]
    fault: Option<FaultPoint>,
}'''
new='''#[derive(Clone)]
pub(crate) struct TaskStore {
    state_root: PathBuf,
    #[cfg(test)]
    fault: Option<FaultPoint>,
}'''
assert old in s
s=s.replace(old,new)
open(p,'w',encoding='utf-8').write(s)
print("ok")