use super::*;

tokio::task_local! {
    pub(in crate::client::interface::questions) static TEST_QUEUE: &'static Mutex<Queue>;
}

/// The production queue API borrows a process-lifetime mutex. Keep only that
/// small test wrapper alive; clear its retained records when the scope ends.
pub(crate) async fn with_test_queue<F: std::future::Future>(future: F) -> F::Output {
    struct Clear(&'static Mutex<Queue>);
    impl Drop for Clear {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = Queue::default();
        }
    }
    let queue: &'static Mutex<Queue> = Box::leak(Box::new(Mutex::new(Queue {
        connection: Some(("question-history-fixture".into(), 7)),
        ..Queue::default()
    })));
    let _clear = Clear(queue);
    TEST_QUEUE.scope(queue, future).await
}

pub(crate) fn enqueue_for_test(finish: bool, text: &str, key: &str) -> QuestionView {
    let owner = capture();
    let scope = RequestScope {
        connection: 7,
        session_id: "question-history-fixture".into(),
        requester: requests::AgentActor {
            provider: "github".into(),
            subject: "question-history-fixture".into(),
            thread_id: format!("requester-{key}"),
            name: "Fixture requester".into(),
        },
    };
    let (question, created) = state()
        .lock()
        .unwrap()
        .ask_owned(
            parse_ask(json!({"question":text,"idempotency_key":key})).unwrap(),
            Some(scope),
            if finish {
                QuestionKind::Finish
            } else {
                QuestionKind::Question
            },
            Some(owner),
        )
        .unwrap();
    if created {
        record_created(&question).unwrap();
    }
    get_view(&question.id).unwrap()
}

#[tokio::test]
async fn retrying_question_from_another_history_keeps_its_first_owner() {
    let directory = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("ssmcp-question-owner-{}", random_id().unwrap()));
    crate::platform::private_directory(&directory).unwrap();
    saved::with_test_history(
        directory.join("history"),
        with_test_queue(async {
            saved::record("user", "Second conversation seed").unwrap();
            let second = saved::current().unwrap().unwrap();
            saved::release_current().unwrap();
            saved::record("user", "First conversation seed").unwrap();
            let original = saved::current_owner().unwrap();
            let first = enqueue_for_test(true, "May I finish the fixture?", "stable-question");
            saved::resume_test_history(&second.metadata.id).unwrap();
            let repeated = enqueue_for_test(true, "May I finish the fixture?", "stable-question");
            assert_eq!(first.question.id, repeated.question.id);
            assert_eq!(owner(&first.question.id).unwrap(), original);
            assert!(
                serde_json::to_value(&repeated)
                    .unwrap()
                    .get("history_owner")
                    .is_none()
            );
            assert_eq!(saved::current().unwrap().unwrap(), second);
            assert_eq!(
                saved::read(original.id())
                    .unwrap()
                    .messages
                    .iter()
                    .filter(|message| message.role == "assistant")
                    .count(),
                1
            );
        }),
    )
    .await;
    assert!(std::fs::read_dir(&directory).unwrap().next().is_none());
    std::fs::remove_dir_all(directory).unwrap();
}
