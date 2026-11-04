use super::*;

// Kernel hook grammars. Registered code and unknown branches stay opaque to
// immediate-execution proofs; selectors and ordering rules remain data.
pub(crate) const HOOK_CONDITIONS: &[&str] = &[
    "IfHookEmptyTF",
    "IfHookEmptyT",
    "IfHookEmptyF",
    "IfHookExistsTF",
];

pub(super) fn kernel_hook_parameters(name: &str) -> Option<&'static [(&'static str, bool)]> {
    Some(match name {
        "IfHookEmptyTF" | "IfHookExistsTF" => {
            &[("hook", false), ("true code", false), ("false code", false)]
        }
        "IfHookEmptyT" => &[("hook", false), ("true code", false)],
        "IfHookEmptyF" => &[("hook", false), ("false code", false)],
        "AddToHook" | "AddToHookWithArguments" => {
            &[("hook", false), ("label", true), ("code", false)]
        }
        "AddToHookNext" | "AddToHookNextWithArguments" => &[("hook", false), ("code", false)],
        "AtBeginDocument" | "AtEndDocument" => &[("label", true), ("code", false)],
        "RemoveFromHook" => &[("hook", false), ("label", true)],
        "NewHook"
        | "NewReversedHook"
        | "UseHook"
        | "UseOneTimeHook"
        | "ClearHookNext"
        | "ActivateGenericHook"
        | "DisableGenericHook"
        | "ShowHook"
        | "LogHook" => &[("hook", false)],
        "UseHookWithArguments" | "UseOneTimeHookWithArguments" => {
            &[("hook", false), ("argument count", false)]
        }
        "NewHookWithArguments" | "NewReversedHookWithArguments" => {
            &[("hook", false), ("argument count", false)]
        }
        "NewMirroredHookPair" => &[("before hook", false), ("after hook", false)],
        "NewMirroredHookPairWithArguments" => &[
            ("before hook", false),
            ("after hook", false),
            ("argument count", false),
        ],
        "DeclareHookRule" => &[
            ("hook", false),
            ("first label", false),
            ("relation", false),
            ("second label", false),
        ],
        "DeclareDefaultHookRule" => &[
            ("first label", false),
            ("relation", false),
            ("second label", false),
        ],
        "ClearHookRule" => &[
            ("hook", false),
            ("first label", false),
            ("second label", false),
        ],
        "SetDefaultHookLabel" | "PushDefaultHookLabel" => &[("label", false)],
        "PopDefaultHookLabel" | "DebugHooksOn" | "DebugHooksOff" => &[],
        _ => return None,
    })
}

pub(super) fn builtin(name: &str) -> Option<Signature> {
    if matches!(name, "assignrefcontextcats" | "assignrefcontextkeyws") {
        let keywords = name == "assignrefcontextkeyws";
        return Some(Signature {
            name: name.into(),
            parameters: vec![
                Parameter { obey_spaces: false, kind: Kind::RequiredBracket, label: "[options]".into() },
                Parameter { obey_spaces: false, kind: Kind::RequiredGroup, label: if keywords { "{keywords}" } else { "{categories}" }.into() },
            ],
            documentation: if keywords {
                "Assign bibliography keywords to a reference context. The options bracket is required, including when empty; the starred form applies weak assignments. Keywords are independent of entry keys and categories. Requires biblatex."
            } else {
                "Assign bibliography categories to a reference context. The options bracket is required, including when empty; the starred form applies weak assignments. Category names are independent of entry keys. Requires biblatex."
            }.into(),
            starred: true,
            nested_brackets: false,
        });
    }
    if let Some(style) = crate::bibliography::citations::style(name) {
        return Some(Signature {
            name: name.into(),
            parameters: citation_parameters(style, 1),
            documentation: if style.context_entries() {
                "Assign bibliography entry keys to a reference context. The options bracket is required, including when empty; the starred form applies weak assignments. This does not cite the entries. Requires biblatex."
            } else if style.category_membership() {
                "Assign bibliography keys to a declared category. This does not cite the entries. Requires biblatex."
            } else if style.set_definition() {
                "Define a dynamic bibliography set with a citation key and a list of member keys. Requires biblatex with Biber."
            } else if style.context_codes() == 2 {
                "Test a bibliography entry by key and provide code for its two branches. Requires biblatex."
            } else if style.single_key() {
                "Execute code in one bibliography entry's data context. Supports the starred form. Requires biblatex."
            } else {
                "Biblatex citation arguments. Repeated calls accept up to two overall notes in parentheses; volume citations separate the volume and optional pages from bibliography keys. Data citations select a field or list after an optional format. Requires biblatex."
            }.into(),
            starred: style.accepts_star(),
            nested_brackets: false,
        });
    }
    let (arguments, starred): (&[(&str, bool)], bool) = match name {
        // Booktabs rules only read an optional width. A following environment
        // delimiter is an independent call, even when no width is supplied.
        "toprule" | "midrule" | "bottomrule" => (&[("width", true)], false),
        "newglossaryentry" | "DeclareAcronym" => (&[("key", false), ("fields", false)], false),
        "longnewglossaryentry" => (
            &[("key", false), ("fields", false), ("description", false)],
            false,
        ),
        "newacronym" | "newabbreviation" => (
            &[
                ("options", true),
                ("key", false),
                ("short", false),
                ("long", false),
            ],
            false,
        ),
        "newacro" | "acrodef" => (&[("key", false), ("short", true), ("long", false)], false),
        "gls" | "Gls" | "glspl" | "Glspl" | "acrshort" | "acrlong" | "acrfull" | "ac" | "acs"
        | "acl" | "acf" => (&[("options", true), ("key", false)], true),
        "glslink" | "glsdisp" => (&[("options", true), ("key", false), ("text", false)], true),
        "index" | "sindex" => (&[("index", true), ("entry", false)], false),
        "makeindex" | "printindex" => (&[("index", true)], false),
        "loadglsentries" => (&[("type", true), ("file", false)], false),
        "externaldocument" => (&[("prefix", true), ("file", false), ("URL", true)], false),
        "import" | "subimport" | "inputfrom" | "subinputfrom" | "includefrom"
        | "subincludefrom" => (&[("directory", false), ("file", false)], true),
        "DeclareBibliographyCategory" => (&[("category", false)], false),
        "printbibliography" => (&[("options", true)], false),
        "newrefsection" | "refsection" => (&[("resources", true)], false),
        "item" => (&[("label", true)], false),
        "ifcategory" => (
            &[("category", false), ("true", false), ("false", false)],
            false,
        ),
        "defbibfilter" => (&[("name", false), ("filter", false)], false),
        "AtBeginRefsection"
        | "AtNextRefsection"
        | "AtFollowingRefsections"
        | "AtBeginBibliography"
        | "AtBeginShorthands"
        | "AtEveryBibitem"
        | "AtEveryLositem"
        | "AtNextBibliography"
        | "AtEveryCite"
        | "AtEveryCitekey"
        | "AtEveryMultiCite"
        | "AtNextCite"
        | "AtEachCitekey"
        | "AtNextCitekey"
        | "AtNextMultiCite" => (&[("code", false)], false),
        "AtUsedriver" | "AtVolcite" => (&[("code", false)], true),
        "AtDataInput" => (&[("entry type", true), ("code", false)], false),
        "AtBeginBiblist" | "AtEveryBiblistitem" => {
            (&[("bibliography list", false), ("code", false)], false)
        }
        "AtEveryEntrykey" => (
            &[("code", false), ("success", false), ("failure", false)],
            false,
        ),
        "printfield" => (&[("format", true), ("field", false)], false),
        "thefield" | "strfield" => (&[("field", false)], false),
        "iffieldundef" => (
            &[("field", false), ("true", false), ("false", false)],
            false,
        ),
        "iflistundef" => (
            &[("literal list", false), ("true", false), ("false", false)],
            false,
        ),
        "ifnameundef" => (
            &[("name list", false), ("true", false), ("false", false)],
            false,
        ),
        "ifentrytype" => (
            &[("entry type", false), ("true", false), ("false", false)],
            false,
        ),
        "iffieldequalstr" => (
            &[
                ("field", false),
                ("string", false),
                ("true", false),
                ("false", false),
            ],
            false,
        ),
        "iffieldequals" => (
            &[
                ("field", false),
                ("command", false),
                ("true", false),
                ("false", false),
            ],
            false,
        ),
        "iffieldsequal" => (
            &[
                ("field 1", false),
                ("field 2", false),
                ("true", false),
                ("false", false),
            ],
            false,
        ),
        "iflistequals" => (
            &[
                ("literal list", false),
                ("command", false),
                ("true", false),
                ("false", false),
            ],
            false,
        ),
        "ifnameequals" => (
            &[
                ("name list", false),
                ("command", false),
                ("true", false),
                ("false", false),
            ],
            false,
        ),
        "printlist" => (
            &[
                ("format", true),
                ("start-stop", true),
                ("literal list", false),
            ],
            false,
        ),
        "printnames" => (
            &[("format", true), ("start-stop", true), ("name list", false)],
            false,
        ),
        name if syntax::declares_traditional_command(name) => (
            &[
                ("command", false),
                ("arguments", true),
                ("default", true),
                ("definition", false),
            ],
            true,
        ),
        "newenvironment" | "renewenvironment" | "provideenvironment" => (
            &[
                ("environment", false),
                ("arguments", true),
                ("default", true),
                ("begin", false),
                ("end", false),
            ],
            true,
        ),
        "NewDocumentEnvironment"
        | "RenewDocumentEnvironment"
        | "ProvideDocumentEnvironment"
        | "DeclareDocumentEnvironment" => (
            &[
                ("environment", false),
                ("arguments", false),
                ("begin", false),
                ("end", false),
            ],
            false,
        ),
        "NewEnviron" | "RenewEnviron" => (
            &[
                ("environment", false),
                ("arguments", true),
                ("default", true),
                ("definition", false),
                ("final code", true),
            ],
            false,
        ),
        "DeclareTextCommand" | "ProvideTextCommand" => (
            &[
                ("command", false),
                ("encoding", false),
                ("arguments", true),
                ("default", true),
                ("definition", false),
            ],
            false,
        ),
        "DeclareTextCommandDefault" | "ProvideTextCommandDefault" => (
            &[
                ("command", false),
                ("arguments", true),
                ("default", true),
                ("definition", false),
            ],
            false,
        ),
        "DeclareTextSymbol" | "DeclareTextAccent" => (
            &[("command", false), ("encoding", false), ("slot", false)],
            false,
        ),
        name if syntax::declares_environment_copy(name) => (
            &[("environment", false), ("source environment", false)],
            false,
        ),
        name if syntax::declares_command_copy(name) => {
            (&[("command", false), ("source command", false)], false)
        }
        name if syntax::declares_document_command(name) => (
            &[
                ("command", false),
                ("argument specification", false),
                ("definition", false),
            ],
            false,
        ),
        "DeclareMathOperator" => (&[("command", false), ("text", false)], true),
        "operatorname" => (&[("text", false)], true),
        "frac" | "dfrac" | "tfrac" | "binom" => {
            (&[("numerator", false), ("denominator", false)], false)
        }
        "sqrt" => (&[("degree", true), ("expression", false)], false),
        "hat" | "widehat" | "tilde" | "widetilde" | "bar" | "vec" | "dot" | "ddot" | "acute"
        | "grave" | "breve" | "check" | "mathring" | "overline" | "underline" | "sqrtsign" => {
            (&[("expression", false)], false)
        }
        "href" => (&[("url", false), ("text", false)], false),
        "url" => (&[("url", false)], false),
        "definecolor" | "providecolor" | "xdefinecolor" => (
            &[
                ("type", true),
                ("name", false),
                ("prefix", true),
                ("model", false),
                ("specification", false),
            ],
            false,
        ),
        "colorlet" => (
            &[
                ("type", true),
                ("name", false),
                ("model", true),
                ("color", false),
            ],
            false,
        ),
        "color" | "pagecolor" => (&[("model", true), ("color", false)], false),
        "rowcolor" | "columncolor" => (
            &[
                ("model", true),
                ("color", false),
                ("left overhang", true),
                ("right overhang", true),
            ],
            false,
        ),
        "cellcolor" | "arrayrulecolor" | "doublerulesepcolor" | "maskcolors" => {
            (&[("model", true), ("color", false)], false)
        }
        "blendcolors" => (&[("color expression", false)], true),
        "textcolor" | "colorbox" => (&[("model", true), ("color", false), ("text", false)], false),
        "fcolorbox" => (
            &[
                ("frame model", true),
                ("frame color", false),
                ("background model", true),
                ("background color", false),
                ("text", false),
            ],
            false,
        ),
        "includegraphics" => (&[("options", true), ("file", false)], true),
        name if crate::parser::fancyvrb::is_input(name) => {
            (&[("options", true), ("file", false)], true)
        }
        "verbatiminput" => (&[("file", false)], true),
        "lstinputlisting" => (&[("options", true), ("file", false)], false),
        "lstinline" => (&[("options", true)], false),
        "Verb" => (&[("options", true)], true),
        "EscVerb" => (&[("options", true), ("code", false)], true),
        name if crate::parser::fancyvrb::is_inline_save(name)
            || crate::parser::fancyvrb::is_use(name) =>
        {
            (&[("options", true), ("name", false)], true)
        }
        "fvset" | "fvinlineset" => (&[("options", false)], false),
        "mintinline" | "mint" => (
            &[("options", true), ("language", false), ("code", false)],
            false,
        ),
        "inputminted" => (
            &[("options", true), ("language", false), ("file", false)],
            false,
        ),
        "documentclass" | "usepackage" | "RequirePackage" | "LoadClass" => (
            &[
                ("options", true),
                ("name", false),
                ("minimum version", true),
            ],
            false,
        ),
        "ProvidesPackage" | "ProvidesClass" | "ProvidesFile" | "NeedsTeXFormat" => {
            (&[("name", false), ("release information", true)], false)
        }
        name if syntax::loader_forwards_options(name) => {
            (&[("name", false), ("minimum version", true)], false)
        }
        "input" | "include" | "subfile" | "bibliography" | "bibliographystyle" => {
            (&[("file", false)], false)
        }
        "InputIfFileExists" => (
            &[
                ("file", false),
                ("before input", false),
                ("if missing", false),
            ],
            false,
        ),
        "IfFileExists" => (
            &[("file", false), ("if exists", false), ("if missing", false)],
            false,
        ),
        "PassOptionsToPackage" | "PassOptionsToClass" => {
            (&[("options", false), ("name", false)], false)
        }
        "includeonly" => (&[("files", false)], false),
        "undef" | "gundef" | "csuse" | "csmeaning" | "csundef" | "csgundef" => {
            (&[("command", false)], false)
        }
        "cslet" | "letcs" | "csletcs" => (&[("alias", false), ("command", false)], false),
        name if crate::symbols::macro_mutations::COMMANDS.contains(&name) => {
            (crate::symbols::macro_mutations::parameters(name)?, false)
        }
        "ifcsdef" | "ifcsundef" | "ifcsvoid" | "ifcsmacro" | "ifcsparam" | "ifcsprotected"
        | "ifcsltxprotect" | "ifcscounter" | "ifcslength" | "ifcsdimen" | "ifdefcounter"
        | "ifdeflength" | "ifdefdimen" => (
            &[("command", false), ("then", false), ("else", false)],
            false,
        ),
        "ifcsstring" => (
            &[
                ("command", false),
                ("string", false),
                ("then", false),
                ("else", false),
            ],
            false,
        ),
        "ifcsequal" | "ifcsstrequal" => (
            &[
                ("first command", false),
                ("second command", false),
                ("then", false),
                ("else", false),
            ],
            false,
        ),
        "addbibresource" | "addglobalbib" | "addsectionbib" => {
            (&[("options", true), ("file", false)], false)
        }
        "ExecuteBibliographyOptions" => (&[("entry types", true), ("options", false)], false),
        "part" | "chapter" | "section" | "subsection" | "subsubsection" | "paragraph"
        | "subparagraph" | "caption" => (&[("short title", true), ("title", false)], true),
        "footnote" | "footnotetext" => (&[("number", true), ("text", false)], false),
        "footnotemark" => (&[("number", true)], false),
        "textbf" | "textit" | "texttt" | "emph" | "title" | "author" | "date" | "mkbibparens"
        | "mkbibbrackets" | "parentext" | "brackettext" | "mkbibfootnote" | "mkbibfootnotetext" => {
            (&[("text", false)], false)
        }
        "typeout" | "message" => (&[("text", false)], false),
        "extractcolorspec" => (&[("color", false), ("specification command", false)], false),
        "extractcolorspecs" => (
            &[
                ("color", false),
                ("model command", false),
                ("specification command", false),
            ],
            false,
        ),
        "convertcolorspec" => (
            &[
                ("source model", false),
                ("color specification", false),
                ("target model", false),
                ("output command", false),
            ],
            false,
        ),
        "begin" | "end" => (&[("environment", false)], false),
        "label" => (&[("key", false)], false),
        "hyperref" => (&[("key", true), ("text", false)], false),
        "hypertarget" | "hyperlink" => (&[("target", false), ("text", false)], false),
        "pdfbookmark" => (&[("level", true), ("title", false), ("name", false)], false),
        "hyperdef" => (
            &[
                ("key", true),
                ("category", false),
                ("name", false),
                ("text", false),
            ],
            false,
        ),
        "newcounter" => (&[("name", false), ("within", true)], false),
        "newboolean" | "provideboolean" | "newbool" | "providebool" | "boolean" | "booltrue"
        | "boolfalse" => (&[("boolean", false)], false),
        "setboolean" | "setbool" => (&[("boolean", false), ("value", false)], false),
        "newtoggle" | "providetoggle" | "toggletrue" | "togglefalse" => {
            (&[("toggle", false)], false)
        }
        "settoggle" => (&[("toggle", false), ("value", false)], false),
        "iftoggle" | "nottoggle" => (
            &[("toggle", false), ("then", false), ("else", false)],
            false,
        ),
        "ifbool" | "notbool" => (
            &[("boolean", false), ("then", false), ("else", false)],
            false,
        ),
        "ifstrequal" => (
            &[
                ("string", false),
                ("string", false),
                ("then", false),
                ("else", false),
            ],
            false,
        ),
        "ifstrempty" | "ifblank" | "notblank" => (
            &[("string", false), ("then", false), ("else", false)],
            false,
        ),
        "ifthenelse" => (
            &[("condition", false), ("then", false), ("else", false)],
            false,
        ),
        "ifboolexpr" | "ifboolexpe" => (
            &[("condition", false), ("then", false), ("else", false)],
            false,
        ),
        "whileboolexpr" | "unlessboolexpr" => (&[("condition", false), ("body", false)], false),
        "newlength" => (&[("length", false)], false),
        "setlength" | "addtolength" => (&[("length", false), ("value", false)], false),
        "deflength" => (&[("length", false), ("expression", false)], false),
        "settowidth" | "settoheight" | "settodepth" => {
            (&[("length", false), ("text", false)], false)
        }
        "newsavebox" | "usebox" => (&[("box", false)], false),
        "sbox" => (&[("box", false), ("text", false)], false),
        "savebox" => (
            &[
                ("box", false),
                ("width", true),
                ("position", true),
                ("text", false),
            ],
            false,
        ),
        "newfont" => (&[("command", false), ("font", false)], false),
        "DeclareFontEncoding" => (
            &[
                ("encoding", false),
                ("text settings", false),
                ("math settings", false),
            ],
            false,
        ),
        "DeclareFontEncodingDefaults" => {
            (&[("text settings", false), ("math settings", false)], false)
        }
        "DeclareFontFamily" => (
            &[
                ("encoding", false),
                ("family", false),
                ("loading settings", false),
            ],
            false,
        ),
        "DeclareFontShape" => (
            &[
                ("encoding", false),
                ("family", false),
                ("series", false),
                ("shape", false),
                ("size specification", false),
                ("loading settings", false),
            ],
            false,
        ),
        "DeclareFontSubstitution" => (
            &[
                ("encoding", false),
                ("family", false),
                ("series", false),
                ("shape", false),
            ],
            false,
        ),
        "DeclareMathSizes" => (
            &[
                ("text size", false),
                ("math size", false),
                ("script size", false),
                ("scriptscript size", false),
            ],
            true,
        ),
        "fontencoding" => (&[("encoding", false)], false),
        "fontfamily" => (&[("family", false)], false),
        "fontseries" => (&[("series", false)], false),
        "fontshape" => (&[("shape", false)], false),
        "fontsize" => (&[("size", false), ("baseline skip", false)], false),
        "linespread" => (&[("factor", false)], false),
        "usefont" => (
            &[
                ("encoding", false),
                ("family", false),
                ("series", false),
                ("shape", false),
            ],
            false,
        ),
        "DeclareTextFontCommand" => (&[("command", false), ("font commands", false)], false),
        "DeclareFixedFont" => (
            &[
                ("command", false),
                ("encoding", false),
                ("family", false),
                ("series", false),
                ("shape", false),
                ("size", false),
            ],
            false,
        ),
        "DeclareMathAlphabet" => (
            &[
                ("command", false),
                ("encoding", false),
                ("family", false),
                ("series", false),
                ("shape", false),
            ],
            false,
        ),
        "DeclareSymbolFontAlphabet" => (&[("command", false), ("symbol font", false)], false),
        "DeclareMathSymbol" | "DeclareMathAccent" => (
            &[
                ("command", false),
                ("class", false),
                ("symbol font", false),
                ("slot", false),
            ],
            false,
        ),
        "DeclareMathRadical" => (
            &[
                ("command", false),
                ("small symbol font", false),
                ("small slot", false),
                ("large symbol font", false),
                ("large slot", false),
            ],
            false,
        ),
        "DeclareMathDelimiter" => (
            &[
                ("command", false),
                ("class", false),
                ("small symbol font", false),
                ("small slot", false),
                ("large symbol font", false),
                ("large slot", false),
            ],
            false,
        ),
        "DeclarePairedDelimiter" => (
            &[("command", false), ("left", false), ("right", false)],
            false,
        ),
        "DeclarePairedDelimiterX" => (
            &[
                ("command", false),
                ("arguments", true),
                ("left", false),
                ("right", false),
                ("body", false),
            ],
            false,
        ),
        "DeclarePairedDelimiterXPP" => (
            &[
                ("command", false),
                ("arguments", true),
                ("before", false),
                ("left", false),
                ("right", false),
                ("after", false),
                ("body", false),
            ],
            false,
        ),
        name if syntax::declares_font_switch(name) => (
            &[
                ("command", false),
                ("features", true),
                ("font", false),
                ("features", true),
            ],
            false,
        ),
        "newaliascnt" => (&[("alias", false), ("counter", false)], false),
        "setcounterref" | "addtocounterref" | "setcounterpageref" | "addtocounterpageref" => {
            (&[("counter", false), ("key", false)], false)
        }
        "setcounter" | "addtocounter" => (&[("counter", false), ("value", false)], false),
        "defcounter" => (&[("counter", false), ("expression", false)], false),
        "ifltxcounter" => (
            &[("counter", false), ("then", false), ("else", false)],
            false,
        ),
        "stepcounter" | "refstepcounter" | "usecounter" | "value" | "arabic" | "roman"
        | "Roman" | "alph" | "Alph" | "fnsymbol" | "aliascntresetthe" => {
            (&[("counter", false)], false)
        }
        "counterwithin" | "counterwithout" => (
            &[("format", true), ("counter", false), ("within", false)],
            true,
        ),
        "numberwithin" => (
            &[("format", true), ("counter", false), ("within", false)],
            false,
        ),
        "getrefnumber" | "getpagerefnumber" | "refused" => (&[("key", false)], false),
        "getrefbykeydefault" => (
            &[("key", false), ("field", false), ("default", false)],
            false,
        ),
        "IfRefUndefinedExpandable" | "IfRefUndefinedBabel" => (
            &[("key", false), ("undefined", false), ("defined", false)],
            false,
        ),
        "vref" | "Vref" => (&[("same-page", true), ("key", false)], true),
        "vpageref" => (
            &[("same-page", true), ("prefix", true), ("key", false)],
            true,
        ),
        "fullref" => (&[("key", false)], false),
        "vrefrange" | "vpagerefrange" => (
            &[("same-page", true), ("first", false), ("last", false)],
            syntax::reference_accepts_star(name),
        ),
        "crefrange" | "Crefrange" | "cpagerefrange" | "Cpagerefrange" => {
            (&[("first", false), ("last", false)], true)
        }
        name if syntax::is_reference(name) => {
            (&[("keys", false)], syntax::reference_accepts_star(name))
        }
        "bibitem" => (&[("label", true), ("key", false)], false),
        "bibentry" => (&[("key", false)], false),
        "nobibliography" => (&[("databases", false)], false),
        "defcitealias" => (&[("key", false), ("text", false)], false),
        "citenum" => (&[("keys", false)], false),
        "citeyearpar" | "citefullauthor" | "citetalias" | "citepalias" => (
            &[
                ("note / prenote", true),
                ("postnote", true),
                ("keys", false),
            ],
            false,
        ),
        name if syntax::is_citation(name) && name != "nocite" => (
            &[
                ("note / prenote", true),
                ("postnote", true),
                ("keys", false),
            ],
            syntax::citation_accepts_star(name),
        ),
        "nocite" => (&[("keys", false)], false),
        _ => (kernel_hook_parameters(name)?, false),
    };
    let parameters = arguments
        .iter()
        .enumerate()
        .map(|(index, (label, optional))| Parameter {
            obey_spaces: false,
            kind: if index == 1
                && matches!(
                    name,
                    "DeclarePairedDelimiterX" | "DeclarePairedDelimiterXPP"
                ) {
                Kind::RequiredBracket
            } else if *optional {
                if matches!(
                    name,
                    "part"
                        | "chapter"
                        | "section"
                        | "subsection"
                        | "subsubsection"
                        | "paragraph"
                        | "subparagraph"
                ) {
                    Kind::UnstarredOptional
                } else {
                    Kind::Optional
                }
            } else if crate::symbols::macro_mutations::COMMANDS.contains(&name)
                && !name.starts_with("cs")
                && index == usize::from(name == "patchcmd")
                || syntax::declares_command_copy(name)
                || name == "cslet" && index == 1
                || (matches!(
                    name,
                    "letcs"
                        | "undef"
                        | "gundef"
                        | "deflength"
                        | "ifdefcounter"
                        | "ifdeflength"
                        | "ifdefdimen"
                ) || syntax::declares_traditional_command(name)
                    || syntax::declares_document_command(name)
                    || syntax::declares_text_command(name)
                    || syntax::declares_font_switch(name)
                    || syntax::declares_paired_delimiter(name)
                    || matches!(
                        name,
                        "DeclareMathOperator"
                            | "DeclareTextFontCommand"
                            | "DeclareFixedFont"
                            | "DeclareMathAlphabet"
                            | "DeclareSymbolFontAlphabet"
                            | "DeclareMathSymbol"
                            | "DeclareMathAccent"
                            | "DeclareMathRadical"
                            | "DeclareMathDelimiter"
                    ))
                    && index == 0
            {
                Kind::CommandToken
            } else if matches!(name, "verbatiminput" | "EscVerb")
                || crate::parser::minted::is_inline(name)
                || name == "inputminted"
                || syntax::is_range_reference(name)
                || index == 1
                    && matches!(
                        name,
                        "definecolor"
                            | "providecolor"
                            | "xdefinecolor"
                            | "colorlet"
                            | "textcolor"
                            | "colorbox"
                    )
                || name == "fcolorbox" && matches!(index, 1 | 3)
            {
                // Keep range endpoints aligned with reference navigation and
                // color names aligned with their grouped declaration/wrappers.
                // Unsupported stars and extra notes are not bare names.
                Kind::RequiredGroup
            } else {
                Kind::Required
            },
            label: if *optional {
                format!("[{label}]")
            } else {
                format!("{{{label}}}")
            },
        })
        .collect();
    let documentation = COMMANDS
        .iter()
        .find(|(command, _, _)| *command == name)
        .map(|(_, _, documentation)| *documentation)
        .unwrap_or("LaTeX command arguments.")
        .into();
    Some(Signature {
        name: name.into(),
        parameters,
        documentation,
        starred,
        nested_brackets: !(kernel_hook_parameters(name).is_some()
            || matches!(name, "lstinputlisting" | "lstinline")
            || crate::bibliography::bib_resources::references_file(name)
            || crate::parser::minted::is_inline(name)
            || name == "inputminted"
            || crate::parser::fancyvrb::is_input(name)
            || crate::parser::fancyvrb::is_inline(name)
            || crate::parser::fancyvrb::is_use(name)
            || syntax::is_citation(name)
            || syntax::loader_extension(name).is_some()
            || syntax::declares_traditional_command(name)
            || syntax::reference_note_count(name) > 0
            || matches!(
                name,
                "hyperref"
                    | "definecolor"
                    | "providecolor"
                    | "xdefinecolor"
                    | "colorlet"
                    | "color"
                    | "textcolor"
                    | "colorbox"
                    | "fcolorbox"
                    | "pagecolor"
                    | "rowcolor"
                    | "columncolor"
                    | "cellcolor"
                    | "arrayrulecolor"
                    | "doublerulesepcolor"
                    | "maskcolors"
                    | "ProvidesPackage"
                    | "ProvidesClass"
                    | "ProvidesFile"
                    | "NeedsTeXFormat"
                    | "bibitem"
                    | "printfield"
                    | "printlist"
                    | "printnames"
                    | "newenvironment"
                    | "renewenvironment"
                    | "provideenvironment"
                    | "NewEnviron"
                    | "RenewEnviron"
                    | "hyperdef"
                    | "newcounter"
                    | "sqrt"
                    | "part"
                    | "chapter"
                    | "section"
                    | "subsection"
                    | "subsubsection"
                    | "paragraph"
                    | "subparagraph"
                    | "caption"
                    | "footnote"
                    | "footnotetext"
                    | "footnotemark"
                    | "AtDataInput"
                    | "item"
                    | "toprule"
                    | "midrule"
                    | "bottomrule"
                    | "printbibliography"
                    | "ExecuteBibliographyOptions"
                    | "patchcmd"
                    | "newrefsection"
                    | "refsection"
            )),
    })
}
