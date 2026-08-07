use crate::ModelClient;
use crate::ModelError;
use crate::ModelStream;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream;
use papermachine_protocol::HostedTool;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelRequest;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Clone, Default)]
pub struct ScriptedModelClient {
    responses: Arc<Mutex<VecDeque<Vec<ModelEvent>>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl ScriptedModelClient {
    pub fn new(responses: impl IntoIterator<Item = Vec<ModelEvent>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn push_response(&self, response: Vec<ModelEvent>) -> Result<(), ModelError> {
        let mut responses = self
            .responses
            .lock()
            .map_err(|_| ModelError::Stream("scripted response lock poisoned".to_string()))?;
        responses.push_back(response);
        Ok(())
    }

    pub fn requests(&self) -> Result<Vec<ModelRequest>, ModelError> {
        self.requests
            .lock()
            .map(|requests| requests.clone())
            .map_err(|_| ModelError::Stream("scripted request lock poisoned".to_string()))
    }
}

#[async_trait]
impl ModelClient for ScriptedModelClient {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ModelError> {
        self.requests
            .lock()
            .map_err(|_| ModelError::Stream("scripted request lock poisoned".to_string()))?
            .push(request);

        let response = self
            .responses
            .lock()
            .map_err(|_| ModelError::Stream("scripted response lock poisoned".to_string()))?
            .pop_front()
            .ok_or(ModelError::ScriptExhausted)?;

        Ok(stream::iter(response.into_iter().map(Ok)).boxed())
    }

    fn supports_hosted_tool(&self, _model: &str, _tool: HostedTool) -> bool {
        true
    }
}
