//! Fixed-origin, bounded GitHub REST requests. Response bodies never enter errors.
use super::*;
use reqwest::{Method, blocking::Client, header};

const MAX_RESPONSE: u64 = 2 * 1024 * 1024;
pub(super) struct Github {
    pub(super) target: TaskGitHostingTarget,
    pub(super) token: SecretLiteral,
    client: Client,
    #[cfg(test)]
    origin: Option<String>,
    #[cfg(test)]
    pub(super) transport: Option<String>,
}
pub(super) struct Pull {
    pub number: u64,
    pub oid: String,
    pub state: TaskGitPullRequestState,
    pub title: String,
    pub body: String,
}
#[derive(Deserialize)]
struct Repository {
    id: u64,
    full_name: String,
}
#[derive(Deserialize)]
struct Reference {
    #[serde(rename = "ref")]
    name: String,
    object: GitObject,
}
#[derive(Deserialize)]
struct GitObject {
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}
#[derive(Deserialize)]
struct PullWire {
    number: u64,
    state: String,
    merged_at: Option<String>,
    title: String,
    body: Option<String>,
    head: Endpoint,
    base: Endpoint,
}
#[derive(Deserialize)]
struct Endpoint {
    #[serde(rename = "ref")]
    branch: String,
    sha: String,
    repo: Option<RepositoryId>,
}
#[derive(Deserialize)]
struct RepositoryId {
    id: u64,
}
impl Github {
    pub(super) fn new(
        target: TaskGitHostingTarget,
        token: SecretLiteral,
    ) -> Result<Self, TaskGitHostingError> {
        target.validate()?;
        if token.expose_secret().is_empty() || token.expose_secret().len() > 65536 {
            return Err(TaskGitHostingError::Invalid("GitHub credential"));
        }
        let client = Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(clean)?;
        Ok(Self {
            target,
            token,
            client,
            #[cfg(test)]
            origin: None,
            #[cfg(test)]
            transport: None,
        })
    }
    #[cfg(test)]
    pub(super) fn loopback(
        target: TaskGitHostingTarget,
        token: SecretLiteral,
        origin: String,
    ) -> Self {
        assert!(origin.starts_with("http://127.0.0.1:"));
        let mut client = Self::new(target, token).unwrap();
        client.client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        client.origin = Some(origin);
        client
    }
    pub(super) fn git_url(&self) -> String {
        #[cfg(test)]
        if let Some(url) = &self.transport {
            return url.clone();
        }
        self.target.git_url()
    }
    fn url(&self, suffix: &[&str]) -> reqwest::Url {
        #[cfg(test)]
        let origin = self.origin.as_deref().unwrap_or("https://api.github.com");
        #[cfg(not(test))]
        let origin = "https://api.github.com";
        let mut url = reqwest::Url::parse(origin).expect("fixed GitHub origin");
        url.path_segments_mut()
            .expect("HTTP origin")
            .extend(["repos", &self.target.owner, &self.target.repository])
            .extend(suffix);
        url
    }
    fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        suffix: &[&str],
        query: &[(&str, String)],
        body: Option<&serde_json::Value>,
        operation: &Operation,
        missing: bool,
    ) -> Result<Option<T>, TaskGitHostingError> {
        operation.check()?;
        let remaining = operation
            .deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(30));
        let mut authorization =
            header::HeaderValue::from_str(&format!("Bearer {}", self.token.expose_secret()))
                .map_err(|_| TaskGitHostingError::Invalid("GitHub credential"))?;
        authorization.set_sensitive(true);
        let mut url = self.url(suffix);
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(key, value)| (*key, value.as_str())));
        }
        let mut request = self
            .client
            .request(method, url)
            .timeout(remaining)
            .header(header::AUTHORIZATION, authorization)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header(header::USER_AGENT, "qq-task-git")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().map_err(clean)?;
        if missing && response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(TaskGitHostingError::Status(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE)
        {
            return Err(TaskGitHostingError::Invalid("GitHub response too large"));
        }
        let mut bytes = Vec::new();
        response.take(MAX_RESPONSE + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_RESPONSE {
            return Err(TaskGitHostingError::Invalid("GitHub response too large"));
        }
        operation.check()?;
        Ok(Some(serde_json::from_slice(&bytes).map_err(|_| {
            TaskGitHostingError::Invalid("GitHub response schema")
        })?))
    }
    pub(super) fn repository(&self, operation: &Operation) -> Result<u64, TaskGitHostingError> {
        let repo: Repository = self
            .request(Method::GET, &[], &[], None, operation, false)?
            .ok_or(TaskGitHostingError::Invalid("repository missing"))?;
        if repo.id == 0
            || !repo
                .full_name
                .eq_ignore_ascii_case(&format!("{}/{}", self.target.owner, self.target.repository))
        {
            return Err(TaskGitHostingError::Invalid("GitHub repository identity"));
        }
        Ok(repo.id)
    }
    pub(super) fn head(
        &self,
        head: &str,
        operation: &Operation,
    ) -> Result<Option<String>, TaskGitHostingError> {
        let Some(reference): Option<Reference> = self.request(
            Method::GET,
            &["git", "ref", "heads", head],
            &[],
            None,
            operation,
            true,
        )?
        else {
            return Ok(None);
        };
        if reference.name != format!("refs/heads/{head}") || reference.object.kind != "commit" {
            return Err(TaskGitHostingError::Invalid("GitHub task ref identity"));
        }
        Ok(Some(oid(reference.object.sha)?))
    }
    fn validate_pull(
        &self,
        wire: PullWire,
        repository_id: u64,
        head: &str,
    ) -> Result<Pull, TaskGitHostingError> {
        if wire.number == 0
            || wire.head.branch != head
            || wire.base.branch != self.target.base_branch
            || wire.head.repo.as_ref().map(|r| r.id) != Some(repository_id)
            || wire.base.repo.as_ref().map(|r| r.id) != Some(repository_id)
        {
            return Err(TaskGitHostingError::Invalid("GitHub pull request identity"));
        }
        let head_oid = oid(wire.head.sha)?;
        oid(wire.base.sha)?;
        let state = match (wire.state.as_str(), wire.merged_at) {
            ("closed", Some(_)) => TaskGitPullRequestState::Merged,
            ("closed", None) => TaskGitPullRequestState::Closed,
            ("open", None) => TaskGitPullRequestState::Open,
            _ => return Err(TaskGitHostingError::Invalid("GitHub pull request state")),
        };
        Ok(Pull {
            number: wire.number,
            oid: head_oid,
            state,
            title: wire.title,
            body: wire.body.unwrap_or_default(),
        })
    }
    pub(super) fn pull(
        &self,
        number: u64,
        repository_id: u64,
        head: &str,
        operation: &Operation,
    ) -> Result<Pull, TaskGitHostingError> {
        let wire: PullWire = self
            .request(
                Method::GET,
                &["pulls", &number.to_string()],
                &[],
                None,
                operation,
                false,
            )?
            .ok_or(TaskGitHostingError::Invalid("pull request missing"))?;
        if wire.number != number {
            return Err(TaskGitHostingError::Invalid("pull request number"));
        }
        self.validate_pull(wire, repository_id, head)
    }
    pub(super) fn find_pull(
        &self,
        repository_id: u64,
        head: &str,
        operation: &Operation,
    ) -> Result<Option<Pull>, TaskGitHostingError> {
        let mut found = None;
        for page in 1..=10 {
            let wires: Vec<PullWire> = self
                .request(
                    Method::GET,
                    &["pulls"],
                    &[
                        ("state", "all".into()),
                        ("head", format!("{}:{head}", self.target.owner)),
                        ("base", self.target.base_branch.clone()),
                        ("per_page", "100".into()),
                        ("page", page.to_string()),
                    ],
                    None,
                    operation,
                    false,
                )?
                .ok_or(TaskGitHostingError::Invalid("pull request list missing"))?;
            let count = wires.len();
            for wire in wires {
                let pull = self.validate_pull(wire, repository_id, head)?;
                if found.is_some() {
                    return Err(TaskGitHostingError::Invalid("ambiguous task pull requests"));
                }
                found = Some(pull);
            }
            if count < 100 {
                return Ok(found);
            }
        }
        Err(TaskGitHostingError::Invalid(
            "pull request pagination limit",
        ))
    }
    pub(super) fn create_pull(
        &self,
        title: &str,
        body: &str,
        repository_id: u64,
        head: &str,
        operation: &Operation,
    ) -> Result<Pull, TaskGitHostingError> {
        let wire=self.request(Method::POST,&["pulls"],&[],Some(&serde_json::json!({"title":title,"body":body,"head":head,"base":self.target.base_branch})),operation,false)?.ok_or(TaskGitHostingError::Invalid("pull request missing"))?;
        self.validate_pull(wire, repository_id, head)
    }
    pub(super) fn update_pull(
        &self,
        number: u64,
        title: &str,
        body: &str,
        repository_id: u64,
        head: &str,
        operation: &Operation,
    ) -> Result<Pull, TaskGitHostingError> {
        let wire: PullWire = self
            .request(
                Method::PATCH,
                &["pulls", &number.to_string()],
                &[],
                Some(&serde_json::json!({"title":title,"body":body})),
                operation,
                false,
            )?
            .ok_or(TaskGitHostingError::Invalid("pull request missing"))?;
        if wire.number != number {
            return Err(TaskGitHostingError::Invalid("pull request number"));
        }
        self.validate_pull(wire, repository_id, head)
    }
}
fn clean(error: reqwest::Error) -> TaskGitHostingError {
    TaskGitHostingError::Http(error.without_url())
}
