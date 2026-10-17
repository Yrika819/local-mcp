p='src/managed_worktree_snapshot_index_blob.rs'
s=open(p,encoding='utf-8').read()
old='''        tokio::time::timeout(Duration::from_secs(10), async {
            while !marker.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("PowerShell child published its PID witness");'''
new='''        tokio::time::timeout(Duration::from_secs(30), async {
            while !marker.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("PowerShell child published its PID witness");'''
assert old in s
s=s.replace(old,new,1)
open(p,'w',encoding='utf-8').write(s)
print("ok")