use crate::HttpError;
use std::io::Read;

pub(crate) fn input<T: serde::de::DeserializeOwned>(
    request: &mut tiny_http::Request,
) -> Result<(T, Option<String>), HttpError> {
    let tokens: Vec<_> = request
        .headers()
        .iter()
        .filter(|h| h.field.equiv(git_locator::import::TOKEN_HEADER))
        .map(|h| h.value.as_str().to_owned())
        .collect();
    if tokens.len() > 1 {
        return Err(HttpError::new(400, "duplicate Git token header"));
    }
    let mut body = Vec::new();
    request.as_reader().take(16385).read_to_end(&mut body)?;
    if body.len() > 16384 {
        return Err(HttpError::new(400, "Git request too large"));
    }
    let input =
        serde_json::from_slice(&body).map_err(|_| HttpError::new(400, "invalid Git request"))?;
    Ok((input, tokens.into_iter().next()))
}
