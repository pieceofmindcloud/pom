#![cfg_attr(test, allow(dead_code))]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const GENERATED_FILE: &str = "mtp_graph_architectures.rs";
// Menos de quatro no resultado derivado é tratado como quebra do extrator, não
// como uma capacidade upstream que possa desligar o MTP silenciosamente. Este
// é um piso de segurança, não uma fonte de nomes de arquitetura.
const MIN_EXPECTED_MTP_ARCHITECTURES: usize = 4;
const MTP_GRAPH_TYPE: &str = "LLM_GRAPH_TYPE_DECODER_MTP";

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Identifier(String),
    StringLiteral(String),
    Punctuation(u8),
}

pub(crate) fn generate(llama_cpp_root: &Path, out_dir: &Path) {
    ensure_llama_cpp_root(llama_cpp_root).unwrap_or_else(|error| panic!("{error}"));

    let source_root = llama_cpp_root.join("src");
    let models_root = source_root.join("models");
    let dispatch_path = source_root.join("llama-model.cpp");
    let names_path = source_root.join("llama-arch.cpp");

    println!("cargo:rerun-if-changed={}", models_root.display());
    println!("cargo:rerun-if-changed={}", dispatch_path.display());
    println!("cargo:rerun-if-changed={}", names_path.display());

    let model_sources = read_model_sources(&models_root);
    let model_dispatch = read_required_source(&dispatch_path);
    let architecture_names = read_required_source(&names_path);
    let architectures =
        extract_mtp_architectures(&model_sources, &model_dispatch, &architecture_names)
            .unwrap_or_else(|error| {
                panic!(
                    "não foi possível derivar as arquiteturas MTP do llama.cpp em {}: {error}",
                    llama_cpp_root.display()
                )
            });

    fs::write(
        out_dir.join(GENERATED_FILE),
        render_generated_file(&architectures),
    )
    .unwrap_or_else(|error| {
        panic!(
            "não foi possível escrever {}: {error}",
            out_dir.join(GENERATED_FILE).display()
        )
    });
}

fn ensure_llama_cpp_root(llama_cpp_root: &Path) -> Result<(), String> {
    if llama_cpp_root.is_dir() {
        return Ok(());
    }
    Err(format!(
        "llama.cpp não encontrado em {}; a lista de arquiteturas MTP não pode ser gerada",
        llama_cpp_root.display()
    ))
}

fn read_model_sources(models_root: &Path) -> Vec<(PathBuf, String)> {
    let mut paths = fs::read_dir(models_root)
        .unwrap_or_else(|error| {
            panic!(
                "não foi possível ler a pasta de modelos do llama.cpp em {}: {error}",
                models_root.display()
            )
        })
        .map(|entry| {
            entry
                .unwrap_or_else(|error| {
                    panic!("não foi possível ler uma entrada de models: {error}")
                })
                .path()
        })
        .filter(|path| path.extension().is_some_and(|extension| extension == "cpp"))
        .collect::<Vec<_>>();
    paths.sort();

    paths
        .into_iter()
        .map(|path| {
            let source = read_required_source(&path);
            (path, source)
        })
        .collect()
}

fn read_required_source(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("não foi possível ler {}: {error}", path.display()))
}

pub(crate) fn extract_mtp_architectures(
    model_sources: &[(PathBuf, String)],
    model_dispatch: &str,
    architecture_names: &str,
) -> Result<Vec<String>, String> {
    if model_sources.is_empty() {
        return Err(
            "nenhuma verificação efetiva de LLM_GRAPH_TYPE_DECODER_MTP foi encontrada em src/models"
                .to_string(),
        );
    }
    let dispatch = parse_model_dispatch(model_dispatch)?;
    let names = parse_architecture_names(architecture_names)?;
    let mut architectures = BTreeSet::new();
    let mut checked_files = 0usize;

    for (path, source) in model_sources {
        let tokens = tokenize(source)?;
        if !contains_effective_mtp_check(&tokens) {
            continue;
        }
        checked_files += 1;

        let classes = graph_classes(&tokens);
        if classes.len() != 1 {
            return Err(format!(
                "{} contém uma verificação MTP, mas {} classes build_arch_graph foram encontradas",
                path.display(),
                classes.len()
            ));
        }
        let class = classes.into_iter().next().expect("len == 1");
        let arch = dispatch.get(&class).ok_or_else(|| {
            format!(
                "a classe {class} de {} não foi encontrada no dispatch de llama-model.cpp",
                path.display()
            )
        })?;
        let name = names.get(arch).ok_or_else(|| {
            format!(
                "a arquitetura {arch}, usada pela classe {class}, não foi encontrada na tabela de llama-arch.cpp"
            )
        })?;
        if !architectures.insert(name.clone()) {
            return Err(format!(
                "a arquitetura oficial {name} foi encontrada em mais de um arquivo com grafo MTP"
            ));
        }
    }

    if checked_files == 0 {
        return Err(
            "nenhuma verificação efetiva de LLM_GRAPH_TYPE_DECODER_MTP foi encontrada em src/models"
                .to_string(),
        );
    }

    let architectures = architectures.into_iter().collect::<Vec<_>>();
    validate_mtp_architectures(&architectures)?;
    Ok(architectures)
}

pub(crate) fn validate_mtp_architectures(architectures: &[String]) -> Result<(), String> {
    if architectures.len() < MIN_EXPECTED_MTP_ARCHITECTURES {
        return Err(format!(
            "extração suspeita: {} arquiteturas MTP encontradas; mínimo esperado é {}",
            architectures.len(),
            MIN_EXPECTED_MTP_ARCHITECTURES
        ));
    }
    if architectures
        .iter()
        .any(|architecture| architecture.is_empty())
    {
        return Err("extração suspeita: nome de arquitetura MTP vazio".to_string());
    }
    let unique = architectures.iter().collect::<BTreeSet<_>>();
    if unique.len() != architectures.len() {
        return Err("extração suspeita: nomes de arquitetura MTP duplicados".to_string());
    }
    Ok(())
}

fn render_generated_file(architectures: &[String]) -> String {
    let mut output = String::from("// Gerado por crates/node/build/mtp_arch.rs; não edite.\n");
    output.push_str("pub(crate) const GENERATED_MTP_GRAPH_ARCHITECTURES: &[&str] = &[\n");
    for architecture in architectures {
        writeln!(output, "    {architecture:?},").expect("String não falha ao escrever");
    }
    output.push_str("];\n");
    output
}

fn tokenize(source: &str) -> Result<Vec<Token>, String> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0usize;

    while index < bytes.len() {
        match bytes[index] {
            byte if byte.is_ascii_whitespace() => index += 1,
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                let start = index;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                if index + 1 >= bytes.len() {
                    return Err(format!("comentário C++ não terminado na posição {start}"));
                }
                index += 2;
            }
            b'"' => {
                let (literal, next) = read_quoted(bytes, index, b'"')?;
                tokens.push(Token::StringLiteral(literal));
                index = next;
            }
            b'\'' => {
                let (_, next) = read_quoted(bytes, index, b'\'')?;
                index = next;
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                tokens.push(Token::Identifier(source[start..index].to_string()));
            }
            byte => {
                tokens.push(Token::Punctuation(byte));
                index += 1;
            }
        }
    }

    Ok(tokens)
}

fn read_quoted(bytes: &[u8], start: usize, quote: u8) -> Result<(String, usize), String> {
    let mut index = start + 1;
    let mut value = String::new();
    while index < bytes.len() {
        match bytes[index] {
            byte if byte == quote => return Ok((value, index + 1)),
            b'\\' => {
                let Some(&escaped) = bytes.get(index + 1) else {
                    break;
                };
                value.push(escaped as char);
                index += 2;
            }
            byte => {
                value.push(byte as char);
                index += 1;
            }
        }
    }
    Err(format!(
        "literal entre aspas não terminado na posição {start}"
    ))
}

fn contains_effective_mtp_check(tokens: &[Token]) -> bool {
    tokens.windows(10).any(|window| {
        matches!(
            window,
            [
                Token::Identifier(if_keyword),
                Token::Punctuation(b'('),
                Token::Identifier(params),
                Token::Punctuation(b'.'),
                Token::Identifier(gtype),
                Token::Punctuation(b'='),
                Token::Punctuation(b'='),
                Token::Identifier(mtp_type),
                Token::Punctuation(b')'),
                Token::Punctuation(b'{'),
            ] if if_keyword == "if"
                && params == "params"
                && gtype == "gtype"
                && mtp_type == MTP_GRAPH_TYPE
        )
    }) || tokens.windows(9).any(|window| {
        matches!(
            window,
            [
                Token::Identifier(if_keyword),
                Token::Punctuation(b'('),
                Token::Identifier(params),
                Token::Punctuation(b'.'),
                Token::Identifier(gtype),
                Token::Punctuation(b'='),
                Token::Punctuation(b'='),
                Token::Identifier(mtp_type),
                Token::Punctuation(b')'),
            ] if if_keyword == "if"
                && params == "params"
                && gtype == "gtype"
                && mtp_type == MTP_GRAPH_TYPE
        )
    })
}

fn graph_classes(tokens: &[Token]) -> BTreeSet<String> {
    let mut classes = BTreeSet::new();
    for window in tokens.windows(4) {
        if let [Token::Identifier(class), Token::Punctuation(b':'), Token::Punctuation(b':'), Token::Identifier(function)] =
            window
        {
            if class.starts_with("llama_model_") && function == "build_arch_graph" {
                classes.insert(class.clone());
            }
        }
    }
    classes
}

fn parse_model_dispatch(source: &str) -> Result<BTreeMap<String, String>, String> {
    let tokens = tokenize(source)?;
    let mut dispatch = BTreeMap::new();
    for index in 0..tokens.len() {
        if !is_identifier(tokens.get(index), "case") {
            continue;
        }
        let Some(Token::Identifier(arch)) = tokens.get(index + 1) else {
            continue;
        };
        if !is_punctuation(tokens.get(index + 2), b':') {
            continue;
        }

        let mut cursor = index + 3;
        while cursor + 4 < tokens.len() && !is_identifier(tokens.get(cursor), "case") {
            if is_identifier(tokens.get(cursor), "return")
                && is_identifier(tokens.get(cursor + 1), "new")
                && is_punctuation(tokens.get(cursor + 3), b'(')
            {
                if let Token::Identifier(class) = &tokens[cursor + 2] {
                    dispatch.insert(class.clone(), arch.clone());
                }
                break;
            }
            cursor += 1;
        }
    }

    if dispatch.is_empty() {
        return Err(
            "nenhum mapeamento case LLM_ARCH_* -> llama_model_* foi encontrado".to_string(),
        );
    }
    Ok(dispatch)
}

fn parse_architecture_names(source: &str) -> Result<BTreeMap<String, String>, String> {
    let tokens = tokenize(source)?;
    let table_name = tokens
        .iter()
        .position(|token| is_identifier(Some(token), "LLM_ARCH_NAMES"))
        .ok_or_else(|| {
            "a tabela LLM_ARCH_NAMES não foi encontrada em llama-arch.cpp".to_string()
        })?;
    let table_start = tokens[table_name..]
        .iter()
        .position(|token| is_punctuation(Some(token), b'{'))
        .map(|offset| table_name + offset)
        .ok_or_else(|| "a abertura de LLM_ARCH_NAMES não foi encontrada".to_string())?;
    let table_end = matching_brace(&tokens, table_start)
        .ok_or_else(|| "a tabela LLM_ARCH_NAMES não foi encerrada".to_string())?;
    let mut names = BTreeMap::new();
    for window in tokens[table_start..=table_end].windows(5) {
        if let [Token::Punctuation(b'{'), Token::Identifier(arch), Token::Punctuation(b','), Token::StringLiteral(name), Token::Punctuation(b'}')] =
            window
        {
            if names.insert(arch.clone(), name.clone()).is_some() {
                return Err(format!(
                    "a arquitetura {arch} aparece duas vezes em LLM_ARCH_NAMES"
                ));
            }
        }
    }

    if names.is_empty() {
        return Err(
            "nenhuma entrada { LLM_ARCH_*, nome } foi encontrada em llama-arch.cpp".to_string(),
        );
    }
    Ok(names)
}

fn matching_brace(tokens: &[Token], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        if is_punctuation(Some(token), b'{') {
            depth += 1;
        } else if is_punctuation(Some(token), b'}') {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn is_identifier(token: Option<&Token>, expected: &str) -> bool {
    matches!(token, Some(Token::Identifier(value)) if value == expected)
}

fn is_punctuation(token: Option<&Token>, expected: u8) -> bool {
    matches!(token, Some(Token::Punctuation(value)) if *value == expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llama_cpp_ausente_falha_de_forma_ruidosa() {
        let error = ensure_llama_cpp_root(Path::new("/caminho/que/nao/existe/llama.cpp"))
            .expect_err("um fork ausente não pode gerar uma lista vazia");

        assert!(error.contains("llama.cpp não encontrado"), "erro: {error}");
    }

    fn fixture_sources() -> Vec<(PathBuf, String)> {
        [
            ("bailingmoe3.cpp", "bailingmoe3", "BAILINGMOE3"),
            ("cohere2moe.cpp", "cohere2moe", "COHERE2MOE"),
            ("hy-v3.cpp", "hy_v3", "HY_V3"),
            ("qwen35.cpp", "qwen35", "QWEN35"),
        ]
        .into_iter()
        .map(|(file, class, _)| {
            (
                PathBuf::from(file),
                format!(
                    "std::unique_ptr<llm_graph_context> llama_model_{class}::build_arch_graph(\n\
                        const llm_graph_params & params) const {{\n\
                        if (params.gtype == LLM_GRAPH_TYPE_DECODER_MTP) {{\n\
                            return std::make_unique<graph_mtp>(*this, params);\n\
                        }}\n\
                        return std::make_unique<graph>(*this, params);\n\
                    }}"
                ),
            )
        })
        .collect()
    }

    fn fixture_dispatch() -> &'static str {
        r#"
            case LLM_ARCH_BAILINGMOE3:
                return new llama_model_bailingmoe3(params);
            case LLM_ARCH_COHERE2MOE:
                return new llama_model_cohere2moe(params);
            case LLM_ARCH_HY_V3:
                return new llama_model_hy_v3(params);
            case LLM_ARCH_QWEN35:
                return new llama_model_qwen35(params);
        "#
    }

    fn fixture_names() -> &'static str {
        r#"
            static const std::map<llm_arch, const char *> LLM_ARCH_NAMES = {
            { LLM_ARCH_BAILINGMOE3, "bailingmoe3" },
            { LLM_ARCH_COHERE2MOE, "cohere2moe" },
            { LLM_ARCH_HY_V3, "hy_v3" },
            { LLM_ARCH_QWEN35, "qwen35" },
            };
        "#
    }

    #[test]
    fn extrai_nomes_oficiais_pelo_dispatch_e_pela_tabela() {
        let names =
            extract_mtp_architectures(&fixture_sources(), fixture_dispatch(), fixture_names())
                .expect("fontes MTP válidas devem ser extraídas");

        assert_eq!(
            names,
            vec![
                "bailingmoe3".to_string(),
                "cohere2moe".to_string(),
                "hy_v3".to_string(),
                "qwen35".to_string(),
            ]
        );
    }

    #[test]
    fn mencao_em_comentario_nao_conta_como_verificacao() {
        let mut sources = fixture_sources();
        sources.push((
            PathBuf::from("comentario.cpp"),
            "// if (params.gtype == LLM_GRAPH_TYPE_DECODER_MTP)\n".to_string(),
        ));

        let names = extract_mtp_architectures(&sources, fixture_dispatch(), fixture_names())
            .expect("uma menção em comentário não deve alterar a extração");

        assert_eq!(names.len(), 4);
        assert!(names.iter().any(|name| name == "hy_v3"));
    }

    #[test]
    fn extracao_vazia_falha_de_forma_ruidosa() {
        let error =
            extract_mtp_architectures(&[], "", "").expect_err("lista vazia não pode ser aceita");

        assert!(error.contains("nenhuma"), "erro: {error}");
    }

    #[test]
    fn extracao_visivelmente_pequena_falha_de_forma_ruidosa() {
        let error = validate_mtp_architectures(&[
            "qwen3next".to_string(),
            "qwen35".to_string(),
            "qwen35moe".to_string(),
        ])
        .expect_err("a antiga lista de três arquiteturas é pequena demais");

        assert!(error.contains("suspeita"), "erro: {error}");
    }
}
