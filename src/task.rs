use anyhow::Result;
use diag_trace::LocContextExt;
use serde::{de::DeserializeOwned, Serialize};

/// 任务的稳定标识，用于持久化索引（完成态、指纹等）。
pub trait TaskMeta {
    fn id(&self) -> &'static str;
}

/// 最小任务契约：按值输入/输出，由 `run_task` 统一调度。
pub trait CoreTask: TaskMeta {
    type Input<'a>;
    type Output;
    fn execute<'a>(&self, input: Self::Input<'a>) -> Result<Self::Output>;
}

/// 容器与任务完成标记、落盘的公共能力。
pub trait DataContainerBase {
    fn save(&self) -> anyhow::Result<()>;
    fn mark_task_finish(&mut self, repo_name: &str, task_id: &str) -> anyhow::Result<()>;
    fn is_task_finished(&self, repo_name: &str, task_id: &str) -> anyhow::Result<bool>;
}

/// 为某个 `CoreTask` 提供按 `repo_name` + `task_id` 读写输入输出的映射。
pub trait DataContainer<T: CoreTask>: DataContainerBase {
    fn get_input<'a>(&'a self, repo_name: &str, task_id: &str) -> anyhow::Result<T::Input<'a>>;
    fn save_output(
        &mut self,
        repo_name: &str,
        task_id: &str,
        output: T::Output,
    ) -> anyhow::Result<()>;
}

/// 未完成则取输入、执行任务、写回输出，并标记完成与保存容器。
pub fn run_task<T: CoreTask, C: DataContainer<T>>(
    task: &T,
    container: &mut C,
    repo_name: &str,
) -> anyhow::Result<()> {
    if container.is_task_finished(repo_name, &task.id())? {
        return Ok(());
    }

    let input = container.get_input(repo_name, &task.id())?;
    let output = task.execute(input)?;

    container.save_output(repo_name, &task.id(), output)?;

    container.mark_task_finish(repo_name, &task.id())?;
    container.save()
}

/// 增量任务：业务输入外由框架附带「上次指纹」；是否跳过重复工作由各实现自行判断。
pub trait IncrementalTask: TaskMeta {
    /// 执行任务所需的输入数据
    type Input<'a>;
    /// 任务执行后的产出结果
    type Output;
    /// 用于标识任务输入状态的“指纹”（如哈希值、提交 ID 等）
    type Fingerprint: Serialize + DeserializeOwned;

    /// 执行具体任务逻辑。
    ///
    /// `last_fingerprint` 来自容器持久化的指纹 JSON；反序列化失败时为 `Err`，
    /// 实现通常可将其视为「无有效旧指纹」并照常计算新指纹。
    fn execute<'a>(
        &self,
        input: Self::Input<'a>,
        last_fingerprint: &Result<Self::Fingerprint>,
    ) -> Result<(Self::Output, Self::Fingerprint)>;

    fn fingerprint_to_json(
        &self,
        fingerprint: &Self::Fingerprint,
    ) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(fingerprint)
    }

    fn json_to_fingerprint(
        &self,
        doc: serde_json::Value,
    ) -> Result<Self::Fingerprint, serde_json::Error> {
        serde_json::from_value(doc)
    }
}

impl<T: IncrementalTask> CoreTask for T {
    type Input<'a> = (serde_json::Value, T::Input<'a>);
    type Output = (serde_json::Value, T::Output);
    fn execute<'a>(&self, input: Self::Input<'a>) -> Result<Self::Output> {
        let (fingerprint_json, input) = input;
        let fingerprint = self
            .json_to_fingerprint(fingerprint_json)
            .with_loc_context(|| "Failed to deserialize fingerprint");
        let (output, new_fingerprint) =
            <Self as IncrementalTask>::execute(self, input, &fingerprint)?;
        Ok((self.fingerprint_to_json(&new_fingerprint)?, output))
    }
}

/// 按任务读写指纹 JSON（与 `IncrementalTask` 的序列化类型对应）。
pub trait FingerprintProvider: DataContainerBase {
    fn get_fingerprint_json(
        &self,
        repo_name: &str,
        task_id: &str,
    ) -> anyhow::Result<serde_json::Value>;
    fn save_fingerprint_json(
        &mut self,
        repo_name: &str,
        task_id: &str,
        fingerprint: serde_json::Value,
    ) -> anyhow::Result<()>;
}

/// 增量任务的容器侧映射：业务 `Input`/`Output` 与指纹由框架组合为 `CoreTask` 的元组类型。
pub trait IncrementalDataContainer<T: IncrementalTask>: FingerprintProvider {
    fn get_input<'a>(&'a self, repo_name: &str) -> anyhow::Result<T::Input<'a>>;
    fn save_output(&mut self, repo_name: &str, output: T::Output) -> anyhow::Result<()>;
}

impl<TTask: IncrementalTask, T: IncrementalDataContainer<TTask>> DataContainer<TTask> for T {
    fn get_input<'a>(
        &'a self,
        repo_name: &str,
        task_id: &str,
    ) -> anyhow::Result<(serde_json::Value, TTask::Input<'a>)> {
        let fingerprint = self.get_fingerprint_json(repo_name, task_id)?;
        let input = <Self as IncrementalDataContainer<TTask>>::get_input(self, repo_name)?;
        Ok((fingerprint, input))
    }
    fn save_output(
        &mut self,
        repo_name: &str,
        task_id: &str,
        output: (serde_json::Value, TTask::Output),
    ) -> anyhow::Result<()> {
        let (fingerprint, output) = output;
        self.save_fingerprint_json(repo_name, task_id, fingerprint)?;
        <Self as IncrementalDataContainer<TTask>>::save_output(self, repo_name, output)?;
        Ok(())
    }
}

/// 对多个仓库依次执行元组中声明的一组任务。
pub trait VariadicTaskRunner {
    fn run_one_repo(&mut self, repo_name: &str) -> anyhow::Result<()>;
}

#[macro_export]
macro_rules! impl_variadic_task_runner {
    (&mut $tc:ident, $(&$ty:ident,)+) => {
        impl<'a, 'b> task::VariadicTaskRunner for (&'a mut $tc, $(&'b $ty,)+)
        where
            $tc: $(task::DataContainer<$ty>+)+,
        {
            fn run_one_repo(&mut self, repo_name: &str) -> anyhow::Result<()> {
                #[allow(non_snake_case)]
                let ($tc, $($ty,)+) = self;
                $( task::run_task(*$ty, *$tc, repo_name)?; )+
                Ok(())
            }
        }
    };
}

pub fn run_all<T: VariadicTaskRunner>(runner: &mut T, repo_names: &[String]) -> anyhow::Result<()> {
    for repo_name in repo_names {
        runner.run_one_repo(repo_name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{anyhow, bail};
    use serde::{Deserialize, Serialize};
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct TestContainer {
        is_finished: bool,
        input: i32,
        output: Option<i32>,
        save_error: Option<String>,
        calls: Vec<String>,
        finish_marks: usize,
        fingerprints: HashMap<String, serde_json::Value>,
        incremental_outputs: HashMap<String, i32>,
    }

    struct TestCoreTask {
        fail_on_execute: bool,
    }

    impl TaskMeta for TestCoreTask {
        fn id(&self) -> &'static str {
            "test_core_task"
        }
    }

    impl CoreTask for TestCoreTask {
        type Input<'a> = i32;
        type Output = i32;

        fn execute<'a>(&self, input: Self::Input<'a>) -> Result<Self::Output> {
            if self.fail_on_execute {
                bail!("execute failed");
            }
            Ok(input + 1)
        }
    }

    impl DataContainerBase for TestContainer {
        fn save(&self) -> anyhow::Result<()> {
            if let Some(msg) = &self.save_error {
                bail!("{}", msg);
            }
            Ok(())
        }

        fn mark_task_finish(&mut self, _repo_name: &str, _task_id: &str) -> anyhow::Result<()> {
            self.calls.push("mark_task_finish".to_string());
            self.finish_marks += 1;
            Ok(())
        }

        fn is_task_finished(&self, _repo_name: &str, _task_id: &str) -> anyhow::Result<bool> {
            Ok(self.is_finished)
        }
    }

    impl DataContainer<TestCoreTask> for TestContainer {
        fn get_input<'a>(&'a self, _repo_name: &str, _task_id: &str) -> anyhow::Result<i32> {
            Ok(self.input)
        }

        fn save_output(
            &mut self,
            _repo_name: &str,
            _task_id: &str,
            output: i32,
        ) -> anyhow::Result<()> {
            self.calls.push("save_output".to_string());
            self.output = Some(output);
            Ok(())
        }
    }

    struct TestIncrementalTask {
        seen_fingerprint_is_ok: RefCell<Option<bool>>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct FingerprintDoc {
        value: i32,
    }

    impl TaskMeta for TestIncrementalTask {
        fn id(&self) -> &'static str {
            "test_incremental_task"
        }
    }

    impl IncrementalTask for TestIncrementalTask {
        type Input<'a> = i32;
        type Output = i32;
        type Fingerprint = FingerprintDoc;

        fn execute<'a>(
            &self,
            input: Self::Input<'a>,
            last_fingerprint: &Result<Self::Fingerprint>,
        ) -> Result<(Self::Output, Self::Fingerprint)> {
            self.seen_fingerprint_is_ok
                .replace(Some(last_fingerprint.is_ok()));
            let base = last_fingerprint
                .as_ref()
                .ok()
                .map(|fp| fp.value)
                .unwrap_or_default();
            Ok((input + base, FingerprintDoc { value: input }))
        }
    }

    impl IncrementalDataContainer<TestIncrementalTask> for TestContainer {
        fn get_input<'a>(&'a self, repo_name: &str) -> anyhow::Result<i32> {
            self.incremental_outputs
                .get(repo_name)
                .copied()
                .ok_or_else(|| anyhow!("missing input"))
        }

        fn save_output(&mut self, repo_name: &str, output: i32) -> anyhow::Result<()> {
            self.calls.push("incremental_save_output".to_string());
            self.incremental_outputs
                .insert(repo_name.to_string(), output);
            Ok(())
        }
    }

    impl FingerprintProvider for TestContainer {
        fn get_fingerprint_json(
            &self,
            repo_name: &str,
            task_id: &str,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(self
                .fingerprints
                .get(&format!("{}:{}", repo_name, task_id))
                .cloned()
                .unwrap_or(serde_json::Value::Null))
        }

        fn save_fingerprint_json(
            &mut self,
            repo_name: &str,
            task_id: &str,
            fingerprint: serde_json::Value,
        ) -> anyhow::Result<()> {
            self.calls.push("save_fingerprint_json".to_string());
            self.fingerprints
                .insert(format!("{}:{}", repo_name, task_id), fingerprint);
            Ok(())
        }
    }

    struct RecordingRunner {
        calls: Vec<String>,
        fail_at: Option<String>,
    }

    impl VariadicTaskRunner for RecordingRunner {
        fn run_one_repo(&mut self, repo_name: &str) -> anyhow::Result<()> {
            self.calls.push(repo_name.to_string());
            if self.fail_at.as_ref().is_some_and(|r| r == repo_name) {
                bail!("runner failed at {}", repo_name);
            }
            Ok(())
        }
    }

    #[test]
    fn run_task_skips_when_already_finished() {
        let task = TestCoreTask {
            fail_on_execute: false,
        };
        let mut container = TestContainer {
            is_finished: true,
            input: 1,
            ..Default::default()
        };

        run_task(&task, &mut container, "repo_a").expect("should skip successfully");
        assert!(container.output.is_none());
        assert!(container.calls.is_empty());
        assert_eq!(container.finish_marks, 0);
    }

    #[test]
    fn run_task_executes_and_persists_when_unfinished() {
        let task = TestCoreTask {
            fail_on_execute: false,
        };
        let mut container = TestContainer {
            input: 41,
            ..Default::default()
        };

        run_task(&task, &mut container, "repo_a").expect("run_task should succeed");
        assert_eq!(container.output, Some(42));
        assert_eq!(
            container.calls,
            vec!["save_output".to_string(), "mark_task_finish".to_string()]
        );
        assert_eq!(container.finish_marks, 1);
    }

    #[test]
    fn run_task_propagates_execute_error_without_marking_finished() {
        let task = TestCoreTask {
            fail_on_execute: true,
        };
        let mut container = TestContainer {
            input: 1,
            ..Default::default()
        };

        let err = run_task(&task, &mut container, "repo_a").expect_err("should fail");
        let msg = format!("{:#}", err);
        assert!(msg.contains("execute failed"));
        assert!(container.output.is_none());
        assert!(container.calls.is_empty());
        assert_eq!(container.finish_marks, 0);
    }

    #[test]
    fn run_task_propagates_save_error_after_marking_finished() {
        let task = TestCoreTask {
            fail_on_execute: false,
        };
        let mut container = TestContainer {
            input: 5,
            save_error: Some("save failed".to_string()),
            ..Default::default()
        };

        let err = run_task(&task, &mut container, "repo_a").expect_err("save should fail");
        let msg = format!("{:#}", err);
        assert!(msg.contains("save failed"));
        assert_eq!(container.output, Some(6));
        assert_eq!(
            container.calls,
            vec!["save_output".to_string(), "mark_task_finish".to_string()]
        );
        assert_eq!(container.finish_marks, 1);
    }

    #[test]
    fn incremental_coretask_receives_deserialized_fingerprint() {
        let task = TestIncrementalTask {
            seen_fingerprint_is_ok: RefCell::new(None),
        };
        let fp_json = serde_json::json!({ "value": 10 });

        let (new_fp, output) = CoreTask::execute(&task, (fp_json, 2)).expect("should execute");
        assert_eq!(output, 12);
        assert_eq!(*task.seen_fingerprint_is_ok.borrow(), Some(true));
        assert_eq!(new_fp, serde_json::json!({ "value": 2 }));
    }

    #[test]
    fn incremental_coretask_passes_deserialize_error_to_execute() {
        let task = TestIncrementalTask {
            seen_fingerprint_is_ok: RefCell::new(None),
        };
        let invalid_fp = serde_json::json!("invalid-shape");

        let (new_fp, output) =
            CoreTask::execute(&task, (invalid_fp, 3)).expect("execute should still run");
        assert_eq!(output, 3);
        assert_eq!(*task.seen_fingerprint_is_ok.borrow(), Some(false));
        assert_eq!(new_fp, serde_json::json!({ "value": 3 }));
    }

    #[test]
    fn incremental_data_container_bridge_reads_and_writes_fingerprint_and_output() {
        let task = TestIncrementalTask {
            seen_fingerprint_is_ok: RefCell::new(None),
        };
        let mut container = TestContainer::default();
        container.fingerprints.insert(
            "repo_x:test_incremental_task".to_string(),
            serde_json::json!({ "value": 1 }),
        );
        container
            .incremental_outputs
            .insert("repo_x".to_string(), 5);

        run_task(&task, &mut container, "repo_x").expect("incremental run_task should succeed");
        assert_eq!(container.incremental_outputs.get("repo_x"), Some(&6));
        assert_eq!(
            container
                .fingerprints
                .get("repo_x:test_incremental_task")
                .cloned(),
            Some(serde_json::json!({ "value": 5 }))
        );
        assert_eq!(
            container.calls,
            vec![
                "save_fingerprint_json".to_string(),
                "incremental_save_output".to_string(),
                "mark_task_finish".to_string()
            ]
        );
    }

    #[test]
    fn run_all_runs_in_order() {
        let mut runner = RecordingRunner {
            calls: Vec::new(),
            fail_at: None,
        };
        let repos = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        run_all(&mut runner, &repos).expect("run_all should succeed");
        assert_eq!(
            runner.calls,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn run_all_stops_on_first_error() {
        let mut runner = RecordingRunner {
            calls: Vec::new(),
            fail_at: Some("b".to_string()),
        };
        let repos = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let err = run_all(&mut runner, &repos).expect_err("should fail at b");
        let msg = format!("{:#}", err);
        assert!(msg.contains("runner failed at b"));
        assert_eq!(runner.calls, vec!["a".to_string(), "b".to_string()]);
    }
}
