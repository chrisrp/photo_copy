//! UI translations. The English text is the key: `tr("Cancel")` returns it as is in
//! English and looks it up in the pt-BR table otherwise. Texts with values use named
//! placeholders (`trf("{n} left", &[("n", &count)])`) so a translation can reorder them.
//! Errors that come from the core library stay in English.
//!
//! A test scans the sources for every `tr("...")` / `trf("...")` key and checks that
//! the pt-BR table has it with the same placeholders.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    PtBr,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::En, Lang::PtBr];

    /// Name shown in the language selector.
    pub fn label(self) -> &'static str {
        match self {
            Lang::En => "EN",
            Lang::PtBr => "PT-BR",
        }
    }
}

/// The UI language. Not remembered between runs: the app always starts in English.
static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn lang() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::PtBr,
        _ => Lang::En,
    }
}

pub fn set_lang(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

/// `key` in the current language.
pub fn tr(key: &'static str) -> &'static str {
    translate(lang(), key)
}

/// `key` in the current language with its `{name}` placeholders filled in.
pub fn trf(key: &'static str, args: &[(&str, &dyn Display)]) -> String {
    fill(tr(key), args)
}

fn translate(lang: Lang, key: &'static str) -> &'static str {
    match lang {
        Lang::En => key,
        // A missing entry falls back to English (the source test prevents it).
        Lang::PtBr => pt_br().get(key).copied().unwrap_or(key),
    }
}

fn fill(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut text = template.to_string();
    for (name, value) in args {
        text = text.replace(&format!("{{{name}}}"), &value.to_string());
    }
    text
}

fn pt_br() -> &'static HashMap<&'static str, &'static str> {
    static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    TABLE.get_or_init(|| PT_BR.iter().copied().collect())
}

const PT_BR: &[(&str, &str)] = &[
    // Folders and steps
    ("Destination", "Destino"),
    ("Staging", "Entrada"),
    ("Folders", "Pastas"),
    ("Duplicates", "Duplicatas"),
    ("Organize", "Organizar"),
    ("Both indexed", "Ambas indexadas"),
    ("{side} not indexed", "{side}: sem índice"),
    ("Index both folders", "Indexe as duas pastas"),
    ("Choose folders", "Escolha as pastas"),
    ("No duplicates", "Sem duplicatas"),
    ("{n} left", "{n} restantes"),
    ("Not checked yet", "Ainda não verificado"),
    ("Ready · {n} in staging", "Pronto · {n} na entrada"),
    ("{n} in staging", "{n} na entrada"),
    ("Browse & move staging", "Navegar e mover a entrada"),
    // Folder bar and status line
    ("not selected", "não selecionada"),
    ("{files} files · {age}", "{files} arquivos · {age}"),
    ("not indexed", "sem índice"),
    ("just now", "agora mesmo"),
    ("{n} min ago", "há {n} min"),
    ("{n} h ago", "há {n} h"),
    ("1 day ago", "há 1 dia"),
    ("{n} days ago", "há {n} dias"),
    ("Choose the destination and staging folders.", "Escolha a pasta de destino e a pasta de entrada."),
    ("{side}: {path} ({n} files in saved index)", "{side}: {path} ({n} arquivos no índice salvo)"),
    ("Cannot use {path}: {error}", "Não é possível usar {path}: {error}"),
    ("Indexing {side} folder...", "Indexando a pasta {side}..."),
    ("Indexing stopped unexpectedly.", "A indexação parou inesperadamente."),
    ("Indexing failed: {error}", "Falha na indexação: {error}"),
    ("; {n} skipped, e.g. {first}", "; {n} ignorados, p. ex. {first}"),
    (
        "{side} indexed: {files} files ({hashed} hashed, {reused} unchanged, {removed} removed){skipped}",
        "{side} indexado: {files} arquivos ({hashed} calculados, {reused} sem alteração, {removed} removidos){skipped}",
    ),
    ("Staging checked against the saved indexes.", "Entrada comparada com os índices salvos."),
    ("Choose the destination folder first.", "Escolha a pasta de destino primeiro."),
    ("Moved to {path}", "Movido para {path}"),
    (
        "Copied to {path}, but the original could not be removed ({reason}). Both copies exist.",
        "Copiado para {path}, mas o original não pôde ser removido ({reason}). As duas cópias existem.",
    ),
    (
        "Moved {moved} file(s), skipped {skipped}, failed {failed}.",
        "{moved} arquivo(s) movido(s), {skipped} pulado(s), {failed} com falha.",
    ),
    (
        " {kept} copied but the original could not be removed.",
        " {kept} copiado(s), mas o original não pôde ser removido.",
    ),
    ("Could not move {rel}: {error}", "Não foi possível mover {rel}: {error}"),
    // Step 1
    ("Set up the folders", "Configure as pastas"),
    (
        "Choose the destination (your organized library) and the staging folder (new photos), then index both so duplicates can be found.",
        "Escolha o destino (sua biblioteca organizada) e a pasta de entrada (fotos novas) e depois indexe as duas para encontrar duplicatas.",
    ),
    ("Continue to Duplicates  →", "Continuar para Duplicatas  →"),
    // Step 2
    ("Staging vs destination", "Entrada × destino"),
    (
        "Remove copies until all three are green, then move on to Organize.",
        "Remova as cópias até as três ficarem verdes e depois siga para Organizar.",
    ),
    ("No duplicates left", "Nenhuma duplicata restante"),
    (
        "Destination and staging are clean. {n} staging file(s) are new.",
        "Destino e entrada estão limpos. {n} arquivo(s) da entrada são novos.",
    ),
    ("Continue to Organize  →", "Continuar para Organizar  →"),
    ("{side} is not selected", "{side}: pasta não selecionada"),
    ("{side} is not indexed yet", "{side}: ainda sem índice"),
    ("No duplicates in {side}", "Nenhuma duplicata em {side}"),
    ("{groups} group(s) · {size} reclaimable", "{groups} grupo(s) · {size} recuperáveis"),
    ("{n} identical · {size}", "{n} idênticos · {size}"),
    ("Trash...", "Descartar..."),
    (
        "Compare staging with the destination to find photos you already have.",
        "Compare a entrada com o destino para achar fotos que você já tem.",
    ),
    ("Check staging against destination", "Comparar entrada com destino"),
    ("No staging file is already in the destination", "Nenhum arquivo da entrada já está no destino"),
    (
        "{n} staging file(s) already in the destination",
        "{n} arquivo(s) da entrada já estão no destino",
    ),
    ("same as {path}", "igual a {path}"),
    // Step 3
    ("Duplicates have not been checked yet.", "As duplicatas ainda não foram verificadas."),
    (
        "{n} duplicate(s) still open. Batch moves skip files whose name already exists in the target.",
        "{n} duplicata(s) em aberto. Movimentos em lote pulam arquivos cujo nome já existe no destino.",
    ),
    ("Review duplicates", "Revisar duplicatas"),
    // Dialogs
    ("Batch move finished", "Movimento em lote concluído"),
    (
        "Moved {moved}, skipped {skipped}, failed {failed}, copied but original not removed {kept}.",
        "Movidos {moved}, pulados {skipped}, com falha {failed}, copiados sem remover o original {kept}.",
    ),
    (
        "The files were moved, but an index could not be saved: {error}",
        "Os arquivos foram movidos, mas um índice não pôde ser salvo: {error}",
    ),
    ("COPIED, ORIGINAL NOT REMOVED", "COPIADO, ORIGINAL NÃO REMOVIDO"),
    ("SKIPPED", "PULADO"),
    ("FAILED", "FALHOU"),
    ("OK", "OK"),
    (
        "The file is moved to the .trash folder inside that folder, not deleted.",
        "O arquivo é movido para a pasta .trash dentro daquela pasta, não é apagado.",
    ),
    ("Move to .trash", "Mover para a .trash"),
    (
        "The files are moved to the .trash folder inside their folder, not deleted.",
        "Os arquivos são movidos para a pasta .trash dentro da pasta deles, não são apagados.",
    ),
    ("Moved {n} files to .trash", "{n} arquivos movidos para a .trash"),
    ("Click to preview", "Clique para ver"),
    ("{n} identical", "{n} idênticos"),
    ("Trash both...", "Descartar os dois..."),
    ("Trash all {n}...", "Descartar todos os {n}..."),
    ("Cancel", "Cancelar"),
    // Same photo, different metadata
    ("Same photo, different metadata", "Mesma foto, metadados diferentes"),
    ("{n} group(s)", "{n} grupo(s)"),
    (
        "The picture is the same, but the files differ in their metadata (export ID, GPS, date, thumbnail...). Click a thumbnail to see what differs, then keep the copy you prefer.",
        "A imagem é a mesma, mas os arquivos têm metadados diferentes (ID de exportação, GPS, data, miniatura...). Clique numa miniatura para ver o que muda e fique com a cópia que preferir.",
    ),
    ("{n} copies · metadata differs", "{n} cópias · metadados diferentes"),
    ("same photo as {path} ({size})", "mesma foto que {path} ({size})"),
    (
        "{n} copies of the same photo, metadata differs",
        "{n} cópias da mesma foto, metadados diferentes",
    ),
    ("What differs", "O que muda"),
    ("Field", "Campo"),
    ("(missing)", "(ausente)"),
    ("File size", "Tamanho do arquivo"),
    (
        "No difference in the readable metadata: the files differ in data this app does not show (for example an embedded preview or vendor data).",
        "Nenhuma diferença nos metadados legíveis: os arquivos diferem em dados que o app não mostra (por exemplo uma prévia embutida ou dados do fabricante).",
    ),
    (
        "{folders}: indexed by an older version. Rescan in step 1 to also find copies whose metadata differs (files without that information are read once more).",
        "{folders}: indexado por uma versão anterior. Use \"Verificar de novo\" no passo 1 para encontrar também cópias com metadados diferentes (os arquivos sem essa informação são lidos mais uma vez).",
    ),
    ("No identical files in {side}", "Nenhum arquivo idêntico em {side}"),
    ("Rescan needed", "Verificar de novo"),
    (
        "This index was built by an older version. Rescan once to also find copies of the same photo or video whose metadata differs; files without that information are read again.",
        "Este índice foi criado por uma versão anterior. Verifique de novo uma vez para encontrar também cópias da mesma foto ou vídeo com metadados diferentes; os arquivos sem essa informação são lidos novamente.",
    ),
    // Staging .trash check
    ("Staging .trash", "Lixeira (.trash) da entrada"),
    (
        "Check that every file you trashed from staging has a copy in the destination.",
        "Confira se cada arquivo descartado da entrada tem uma cópia no destino.",
    ),
    (
        "Last check: every trashed file has a copy in the destination.",
        "Última verificação: todo arquivo descartado tem uma cópia no destino.",
    ),
    (
        "Last check: {n} trashed file(s) have no copy in the destination.",
        "Última verificação: {n} arquivo(s) descartado(s) sem cópia no destino.",
    ),
    ("Check .trash...", "Verificar .trash..."),
    ("Check .trash", "Verificar .trash"),
    ("←  Back", "←  Voltar"),
    ("Checking the staging .trash...", "Verificando a .trash da entrada..."),
    ("The .trash check stopped unexpectedly.", "A verificação da .trash parou inesperadamente."),
    (
        ".trash checked: {in} of {total} file(s) are in the destination, {out} are not.",
        ".trash verificada: {in} de {total} arquivo(s) estão no destino, {out} não estão.",
    ),
    ("{n} file(s) read", "{n} arquivo(s) lidos"),
    (
        "{in} of {total} trashed file(s) have a copy in the destination · {out} do not",
        "{in} de {total} arquivo(s) descartado(s) têm cópia no destino · {out} não têm",
    ),
    ("Check again", "Verificar de novo"),
    ("The staging .trash is empty", "A .trash da entrada está vazia"),
    (
        "Every trashed file has a copy in the destination",
        "Todo arquivo descartado tem uma cópia no destino",
    ),
    (
        "These trashed files have no copy in the destination. Click one to see it larger, right-click to open it or show it in the file manager.",
        "Estes arquivos descartados não têm cópia no destino. Clique num deles para ver maior; clique com o botão direito para abrir ou mostrar no gerenciador de arquivos.",
    ),
    // Trash all staging files already in the destination
    ("Trash all {n} from staging...", "Descartar todos os {n} da entrada..."),
    (
        "Each file is checked against its destination copy on disk before it is moved to .trash.",
        "Cada arquivo é comparado com a cópia no destino, lendo do disco, antes de ir para a .trash.",
    ),
    ("Available once indexing has finished.", "Disponível quando a indexação terminar."),
    (
        "Trash {n} staging file(s) already in the destination?",
        "Descartar {n} arquivo(s) da entrada que já estão no destino?",
    ),
    (
        "Each staging file is first compared again with its destination copy, reading both files from disk. It is moved to .trash only if an identical copy still exists in the destination; otherwise it is kept and listed at the end.",
        "Cada arquivo da entrada é comparado de novo com a cópia no destino, lendo os dois arquivos do disco. Ele só vai para a .trash se uma cópia idêntica ainda existir no destino; caso contrário, é mantido e listado no final.",
    ),
    (
        "Files are moved to the .trash folder inside staging, not deleted. A log of every file is saved there too.",
        "Os arquivos são movidos para a pasta .trash dentro da entrada, não são apagados. Um registro de cada arquivo também é salvo lá.",
    ),
    ("Check and move to .trash", "Verificar e mover para a .trash"),
    (
        "Checking staging files against the destination on disk...",
        "Comparando os arquivos da entrada com o destino no disco...",
    ),
    ("Checking and moving to .trash...", "Verificando e movendo para a .trash..."),
    ("{done} of {total}", "{done} de {total}"),
    ("Stop", "Parar"),
    ("Stopping...", "Parando..."),
    ("Trashing stopped unexpectedly.", "O descarte parou inesperadamente."),
    (
        "Nothing was trashed: the log could not be created ({error}).",
        "Nada foi descartado: o registro não pôde ser criado ({error}).",
    ),
    (
        "Moved {trashed} staging file(s) to .trash, kept {kept}, failed {failed}.",
        "{trashed} arquivo(s) da entrada movidos para a .trash, {kept} mantidos, {failed} com falha.",
    ),
    ("Trashing finished", "Descarte concluído"),
    (
        "Moved {trashed} to .trash, kept {kept}, failed {failed}.",
        "{trashed} movidos para a .trash, {kept} mantidos, {failed} com falha.",
    ),
    (
        "Stopped after {done} of {total} files; the rest were not touched.",
        "Parado após {done} de {total} arquivos; o restante não foi alterado.",
    ),
    (
        "The log could not be written, so the batch stopped: {error}",
        "O registro não pôde ser gravado, então o lote parou: {error}",
    ),
    (
        "Some destination files changed since the last index. Rescan the destination in step 1, then check again.",
        "Alguns arquivos do destino mudaram desde a última indexação. Use \"Verificar de novo\" no destino (passo 1) e refaça a comparação.",
    ),
    ("KEPT", "MANTIDO"),
    ("Log:", "Registro:"),
    ("Show log in {app}", "Mostrar registro no {app}"),
    ("the staging file no longer exists", "o arquivo da entrada não existe mais"),
    ("the destination copy no longer exists", "a cópia no destino não existe mais"),
    ("the destination copy is no longer identical", "a cópia no destino não é mais idêntica"),
    // Browser
    ("file manager", "gerenciador de arquivos"),
    ("Could not open {path}: {error}", "Não foi possível abrir {path}: {error}"),
    ("Cannot open {path}: {error}", "Não é possível abrir {path}: {error}"),
    ("Grid", "Grade"),
    ("List", "Lista"),
    ("Choose this folder first.", "Escolha esta pasta primeiro."),
    ("Up", "Acima"),
    ("{n} selected", "{n} selecionado(s)"),
    (
        "Click to select · Cmd/Ctrl adds · Shift selects a range · double-click opens",
        "Clique para selecionar · Cmd/Ctrl adiciona · Shift seleciona um intervalo · clique duplo abre",
    ),
    (
        "Double-click a photo to open it · right-click for more",
        "Clique duplo abre a foto · botão direito para mais opções",
    ),
    ("Move {n} to destination folder...", "Mover {n} para uma pasta do destino..."),
    ("Clear", "Limpar"),
    ("Previous", "Anterior"),
    ("Next", "Próxima"),
    ("Close", "Fechar"),
    ("Move to destination...", "Mover para o destino..."),
    ("This file cannot be previewed here.", "Não é possível pré-visualizar este arquivo aqui."),
    ("Open with the default app", "Abrir com o app padrão"),
    ("This folder has no subfolders or photos.", "Esta pasta não tem subpastas nem fotos."),
    ("Open folder in {app}", "Abrir pasta no {app}"),
    ("Open", "Abrir"),
    ("Show in {app}", "Mostrar no {app}"),
    ("Move {n} selected to destination...", "Mover {n} selecionados para o destino..."),
    ("Name", "Nome"),
    ("Date taken", "Data da foto"),
    ("Size", "Tamanho"),
    ("Kind", "Tipo"),
    // File kinds (from the core library)
    ("Folder", "Pasta"),
    ("JPEG image", "Imagem JPEG"),
    ("PNG image", "Imagem PNG"),
    ("GIF image", "Imagem GIF"),
    ("WebP image", "Imagem WebP"),
    ("BMP image", "Imagem BMP"),
    ("TIFF image", "Imagem TIFF"),
    ("HEIC image", "Imagem HEIC"),
    ("HEIF image", "Imagem HEIF"),
    ("MPEG-4 movie", "Vídeo MPEG-4"),
    ("QuickTime movie", "Vídeo QuickTime"),
    ("AVI movie", "Vídeo AVI"),
    ("File", "Arquivo"),
    // Move dialog
    (
        "That folder no longer exists; showing the top level.",
        "Essa pasta não existe mais; mostrando o nível principal.",
    ),
    ("Cannot read the destination folder: {error}", "Não é possível ler a pasta de destino: {error}"),
    ("Move {n} files to destination", "Mover {n} arquivos para o destino"),
    (
        "{n} of them are identical to files already in the destination.",
        "{n} deles são idênticos a arquivos que já estão no destino.",
    ),
    ("Move to destination", "Mover para o destino"),
    ("From staging: {path}", "Da entrada: {path}"),
    ("Identical file already in the destination: {paths}", "Arquivo idêntico já no destino: {paths}"),
    ("Destination folder", "Pasta de destino"),
    ("(no subfolders)", "(sem subpastas)"),
    ("New folder here:", "Nova pasta aqui:"),
    ("Create", "Criar"),
    (
        "{n} file(s) already have a file with the same name in this folder and will be skipped.",
        "{n} arquivo(s) já têm um arquivo com o mesmo nome nesta pasta e serão pulados.",
    ),
    (
        "Files keep their names. A file whose name is already taken in the destination folder is skipped.",
        "Os arquivos mantêm o nome. Um arquivo cujo nome já existe na pasta de destino é pulado.",
    ),
    ("File name:", "Nome do arquivo:"),
    ("A file with this name already exists in that folder.", "Já existe um arquivo com esse nome nessa pasta."),
    ("Use a free name", "Usar um nome livre"),
    ("Move {n} files here", "Mover {n} arquivos para cá"),
    ("Move here", "Mover para cá"),
    // Step 1 folder rows
    ("your library", "sua biblioteca"),
    ("new photos", "fotos novas"),
    ("Index files", "Indexar arquivos"),
    ("Rescan", "Verificar de novo"),
    (
        "Look for new, changed and removed files and update the index. Only changed files are read again.",
        "Procura arquivos novos, alterados e removidos e atualiza o índice. Só os arquivos alterados são lidos de novo.",
    ),
    (
        "Read every file once so duplicates can be found. Later rescans only read changed files.",
        "Lê cada arquivo uma vez para encontrar duplicatas. As próximas verificações só leem os arquivos alterados.",
    ),
    ("Indexing…", "Indexando…"),
    ("{n} files", "{n} arquivos"),
    ("Not indexed", "Sem índice"),
    ("Choose the destination folder…", "Escolha a pasta de destino…"),
    ("Choose the staging folder…", "Escolha a pasta de entrada…"),
    ("Change…", "Trocar…"),
    ("Change folder…", "Trocar pasta…"),
    ("Change folders or rescan", "Trocar pastas ou verificar de novo"),
    ("Available once both folders are indexed.", "Disponível quando as duas pastas estiverem indexadas."),
    ("Choose it in step 1, Folders.", "Escolha-a no passo 1, Pastas."),
    ("Index it in step 1, Folders.", "Indexe-a no passo 1, Pastas."),
    (
        "a file with this name already exists in the destination folder",
        "já existe um arquivo com esse nome na pasta de destino",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    /// The `{name}` placeholders of a text.
    fn placeholders(text: &str) -> BTreeSet<&str> {
        text.split('{')
            .skip(1)
            .filter_map(|rest| rest.split_once('}').map(|(name, _)| name))
            .collect()
    }

    /// Every string literal passed as the first argument of `tr(` or `trf(`.
    fn keys_in(source: &str) -> Vec<String> {
        let mut keys = Vec::new();
        for call in ["tr(", "trf("] {
            for (start, _) in source.match_indices(call) {
                let preceded_by_ident = source[..start]
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
                if preceded_by_ident {
                    continue;
                }
                let rest = source[start + call.len()..].trim_start();
                let Some(literal) = rest.strip_prefix('"') else {
                    continue;
                };
                let mut key = String::new();
                let mut chars = literal.chars();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => key.extend(chars.next()),
                        '"' => break,
                        c => key.push(c),
                    }
                }
                keys.push(key);
            }
        }
        keys
    }

    #[test]
    fn every_key_in_the_sources_has_a_matching_translation() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0;
        for entry in std::fs::read_dir(src).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|n| n == "i18n.rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            for key in keys_in(&source) {
                let translated = pt_br()
                    .get(key.as_str())
                    .unwrap_or_else(|| panic!("no pt-BR text for {key:?} ({})", path.display()));
                assert_eq!(
                    placeholders(&key),
                    placeholders(translated),
                    "placeholders differ for {key:?}"
                );
                checked += 1;
            }
        }
        assert!(
            checked > 100,
            "only {checked} keys found; is the scan broken?"
        );
    }

    #[test]
    fn table_has_no_duplicate_keys() {
        assert_eq!(pt_br().len(), PT_BR.len());
    }

    #[test]
    fn file_kinds_from_core_are_translated() {
        for ext in [
            "a.jpg", "a.png", "a.gif", "a.webp", "a.bmp", "a.tif", "a.heic", "a.heif", "a.mp4",
            "a.mov", "a.avi", "a.xyz",
        ] {
            let kind = photo_copy_core::meta::kind_label(Path::new(ext));
            assert!(pt_br().contains_key(kind), "no pt-BR text for {kind:?}");
        }
    }

    #[test]
    fn english_is_the_key_and_placeholders_are_filled() {
        assert_eq!(translate(Lang::En, "{n} left"), "{n} left");
        assert_eq!(translate(Lang::PtBr, "{n} left"), "{n} restantes");
        assert_eq!(
            translate(Lang::PtBr, "not in the table"),
            "not in the table"
        );
        let text = fill(
            "{a} and {b}, {a}",
            &[("a", &1 as &dyn Display), ("b", &"two")],
        );
        assert_eq!(text, "1 and two, 1");
    }
}
