//! Prompt contracts. Kept in one place so the protocol stays consistent across
//! API models and local IDE/CLI models.

use crate::protocol::{PlanTask, Proposal};

pub fn architect_system(max_tasks: u32) -> String {
    format!(
        r#"Sen "Komutan" (Architect) modelsin. Bir yazılım projesinin vizyonunu koruyan kıdemli sistem mimarısın.
Görevin: verilen dosya ağacını ve vizyonu analiz edip, işçi modele verilecek KÜÇÜK, TEK DOSYALIK ve UYGULANABİLİR görevler üretmek.

ZORUNLU KURALLAR
1. Yalnızca JSON döndür. JSON dışında tek karakter bile yazma (açıklama, markdown, kod bloğu yok).
2. Şema:
{{"status":"continue"|"complete","summary":"kısa gerekçe","tasks":[{{"id":"T1","title":"kısa başlık","file":"src/ornek.rs","objective":"ne yapılacak, hangi fonksiyon/alan, nasıl","acceptance_criteria":["ölçülebilir kriter"],"context_files":["okunması gereken dosya"],"notes":"dikkat edilecek nokta"}}]}}
3. Her görev TEK dosyaya dokunur. "file" proje köküne göre göreli yoldur, "/" kullan, ".." kullanma.
4. Aynı anda en fazla {max_tasks} görev üret. En küçük anlamlı adımı seç; dev refactor isteme.
5. Vizyonun dışına çıkma. İstenmeyen dosya silme, bağımlılık değişimi, mimari yeniden yazım önerme.
6. Mevcut dosyanın içeriğini bilmiyorsan context_files ile işçiye okut; davranışı bozma.
7. Proje vizyonu karşılandıysa veya yapılacak anlamlı iş kalmadıysa status="complete" ve "tasks":[] döndür.
8. "acceptance_criteria" en az 1, en fazla 4 madde olsun; test edilebilir yaz.
9. Türkçe yaz. JSON string'lerinde satır sonu kullanma.
"#
    )
}

#[allow(clippy::too_many_arguments)]
pub fn plan_prompt(
    vision: &str,
    instructions: &str,
    system_context: &str,
    tree: &str,
    completed: &[String],
    failures: &[String],
    round: u32,
    max_rounds: u32,
    max_tasks: u32,
) -> String {
    let completed_block = if completed.is_empty() {
        "(henüz yok)".to_string()
    } else {
        completed
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let failure_block = if failures.is_empty() {
        "(yok)".to_string()
    } else {
        failures
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let instructions_block = if instructions.trim().is_empty() {
        String::new()
    } else {
        format!(
            "KULLANICI GENEL TALİMATI / İSTEKLERİ\n================\n{}\n\n",
            instructions.trim()
        )
    };

    let system_block = if system_context.trim().is_empty() {
        String::new()
    } else {
        format!("{}\n\n", system_context.trim())
    };

    format!(
        r#"{system_block}{instructions_block}PROJE VİZYONU
================
{vision}

PROJE DOSYA AĞACI (proje köküne göre)
================
{tree}

TAMAMLANAN GÖREVLER
================
{completed_block}

ÖNCEKİ BAŞARISIZLIKLAR / REDLER
================
{failure_block}

DURUM
================
Tur {round}/{max_rounds}. Şimdi sıradaki en değerli en fazla {max_tasks} görevi planla.
Daha önce tamamlanan işi tekrar isteme. Başarısız görevleri aynen tekrarlama; ya daha küçük ve net bir adıma böl ya da farklı bir dosyaya odaklan.
Sadece JSON döndür."#
    )
}

pub fn coder_system() -> String {
    r#"Sen "İşçi" (Coder) modelsin. Sana verilen tek görevi, tek dosyada, eksiksiz uygularsın.

ZORUNLU KURALLAR
1. Sadece istenen dosyayı ve istenen değişikliği yap. Kapsamı büyütme, alakasız dosyalara dokunma.
2. Dosyanın TAM yeni içeriğini üret. Parça, diff veya "mevcut kod aynı kalır" yazma. "..." ile kısaltma yapma.
3. Mevcut çalışan kodu, dili, stil ve bağımlılıkları koru. Sadece görevin gerektirdiğini değiştir.
4. Kod derlenebilir/çalışabilir olmalı. Kullanılan her sembolün import'u ve tanımı dosyada bulunmalı.
5. Yanıt formatı tam olarak şu olmalı:
### FILE: <göreli/yol>
### SUMMARY: <tek cümle>
```<dil>
<dosyanın tam içeriği>
```
6. Kod bloğu dışında açıklama, JSON veya ek metin yazma. Tek bir kod bloğu kullan.
7. Türkçe yorum yazabilirsin; kod kimliği İngilizce kalabilir."#
        .to_string()
}

#[allow(clippy::too_many_arguments)]
pub fn coder_prompt(
    vision: &str,
    instructions: &str,
    system_context: &str,
    task: &PlanTask,
    current: Option<&str>,
    context_files: &str,
    feedback: &str,
    attempt: u32,
    max_attempts: u32,
) -> String {
    let criteria = if task.acceptance_criteria.is_empty() {
        "(belirtilmemiş)".to_string()
    } else {
        task.acceptance_criteria
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let context = if task.context_files.is_empty() {
        "(yok)".to_string()
    } else {
        task.context_files.join(", ")
    };
    let context_block = if context_files.trim().is_empty() {
        String::new()
    } else {
        format!("\n{}", context_files.trim_end())
    };

    let current_block = match current {
        Some(text) if !text.trim().is_empty() => {
            format!(
                "MEVCUT DOSYA İÇERİĞİ (`{}`)\n---8<---\n{text}\n---8<---",
                task.file
            )
        }
        _ => format!("`{}` YENİ BİR DOSYA. Sıfırdan oluştur.", task.file),
    };

    let feedback_block = if feedback.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\nÖNCEKİ DENEME REDDEDİLDİ — BU GERİ BİLDİRİMİ MUTLAKA DÜZELT\n================\n{feedback}\n"
        )
    };

    let instructions_block = if instructions.trim().is_empty() {
        String::new()
    } else {
        format!(
            "KULLANICI GENEL TALİMATI / İSTEKLERİ\n================\n{}\n\n",
            instructions.trim()
        )
    };

    let system_block = if system_context.trim().is_empty() {
        String::new()
    } else {
        format!("{}\n\n", system_context.trim())
    };

    format!(
        r#"{system_block}{instructions_block}PROJE VİZYONU (bağlam)
================
{vision}

GÖREV
================
Kimlik: {id}
Başlık: {title}
Dosya: {file}
Amaç: {objective}
Bağlam dosyaları: {context}
Not: {notes}

KABUL KRİTERLERİ
================
{criteria}
{feedback_block}
DENEME: {attempt}/{max_attempts}

{current_block}
{context_block}

Şimdi `{file}` dosyasının TAM yeni içeriğini, istenen formatta üret."#,
        id = task.id,
        title = task.title,
        file = task.file,
        objective = task.objective,
        context = context,
        notes = if task.notes.trim().is_empty() {
            "-"
        } else {
            task.notes.as_str()
        },
        criteria = criteria,
        feedback_block = feedback_block,
        attempt = attempt,
        max_attempts = max_attempts,
        current_block = current_block,
        context_block = context_block,
    )
}

pub fn reviewer_system() -> String {
    r#"Sen "Komutan" (Architect) modelsin ve denetim kapısısın. İşçinin önerdiği dosya içeriğini göreve, kabul kriterlerine ve proje vizyonuna göre denetlersin.

REDDET (approved=false) eğer:
- Kod derlenmeyecek/çalışmayacak kadar hatalıysa (eksik import, tanımsız sembol, yarım fonksiyon).
- İçerik kesilmişse, "..." / "mevcut kod aynı" gibi eksik bırakma varsa.
- Görev kapsamı dışına çıkılmış, alakasız refactor veya dosya silinmişse.
- Vizyon veya kabul kriterleri karşılanmıyorsa.
- Mevcut davranış bozuluyor veya güvenlik sorunu varsa.
- İçerik boş, anlamsız veya tamamen yeni bir mimari uyduruyorsa.

KURALLAR
1. Yalnızca JSON döndür:
{"approved":true|false,"score":0-10,"summary":"kısa gerekçe","issues":["somut sorun"],"revision":"reddedildiyse işçiye verilecek net, uygulanabilir düzeltme talimatı"}
2. Gerçek bir hata yoksa ve kriterler karşılanıyorsa onayla. Mükemmeliyetçilik yapma.
3. Şüphedeysen reddet; ama reddi somut bir gerekçeye dayandır.
4. Türkçe yaz, JSON dışında metin yazma."#
        .to_string()
}

pub fn review_prompt(
    vision: &str,
    system_context: &str,
    task: &PlanTask,
    proposal: &Proposal,
    current: Option<&str>,
    warnings: &[String],
) -> String {
    let criteria = if task.acceptance_criteria.is_empty() {
        "(belirtilmemiş)".to_string()
    } else {
        task.acceptance_criteria
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let warnings_block = if warnings.is_empty() {
        "(yok)".to_string()
    } else {
        warnings
            .iter()
            .map(|w| format!("- {w}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let system_block = if system_context.trim().is_empty() {
        String::new()
    } else {
        format!("{}\n\n", system_context.trim())
    };

    let current_block = match current {
        Some(text) if !text.trim().is_empty() => {
            format!("MEVCUT DOSYA\n---8<---\n{text}\n---8<---")
        }
        _ => "(dosya yok — yeni oluşturulacak)".to_string(),
    };

    format!(
        r#"{system_block}PROJE VİZYONU
================
{vision}

GÖREV
================
Başlık: {title}
Dosya: {file}
Amaç: {objective}

KABUL KRİTERLERİ
================
{criteria}

YEREL DENETİM UYARILARI
================
{warnings_block}

{current_block}

İŞÇİNİN ÖNERİSİ (`{proposal_file}`)
================
---8<---
{proposed}
---8<---

Bu öneriyi denetle ve sadece JSON karar döndür."#,
        title = task.title,
        file = task.file,
        objective = task.objective,
        criteria = criteria,
        warnings_block = warnings_block,
        current_block = current_block,
        proposal_file = proposal.file,
        proposed = proposal.content,
    )
}
