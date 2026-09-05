using System.Diagnostics;
using System.IO.Compression;
using System.Text;
using System.Text.Json;
using UglyToad.PdfPig.Content;
using UglyToad.PdfPig.Core;
using UglyToad.PdfPig.Fonts.Standard14Fonts;
using UglyToad.PdfPig.Writer;

internal static class MixedCorpusGenerator
{
    // Keep the payload itself searchable. Do not inflate the denominator with opaque padding.
    private const int TargetSearchableChars = 900 * 1024;
    private static readonly JsonSerializerOptions Json = new() { WriteIndented = true };

    public static void Generate(string root, double gib)
    {
        root = Path.GetFullPath(root);
        if (Directory.Exists(root)) throw new IOException("Generator requires a new root; existing corpus is never overwritten");
        Directory.CreateDirectory(root);
        long targetBytes = checked((long)(gib * 1073741824));
        long total = 0; int index = 0; var watch = Stopwatch.StartNew();
        while (total < targetBytes)
        {
            int kind = index % 5;
            string directory = Path.Combine(root, kind switch { 0 => "text", 1 => "docx", 2 => "xlsx", 3 => "pptx", _ => "pdf" }, (index / 1000).ToString("D5"));
            Directory.CreateDirectory(directory);
            string extension = kind switch { 0 => ".txt", 1 => ".docx", 2 => ".xlsx", 3 => ".pptx", _ => ".pdf" };
            string path = Path.Combine(directory, $"mixed_{index:D8}{extension}");
            string text = Content(index, kind == 4);
            switch (kind)
            {
                case 0: CreatePlain(path, text); break;
                case 1: CreateDocx(path, text); break;
                case 2: CreateXlsx(path, text); break;
                case 3: CreatePptx(path, text); break;
                default: CreatePdf(path, text); break;
            }
            total += new FileInfo(path).Length; index++;
            if (index % 250 == 0) Console.WriteLine($"mixed generated {index} files / {total / 1073741824d:F2} GiB ({watch.Elapsed.TotalSeconds:F1}s)");
        }
        File.WriteAllText(root + ".manifest.json", JsonSerializer.Serialize(new
        {
            generator = "astra-mixed-v2-searchable", seed = 20260905, files = index, sourceBytes = total, requestedGiB = gib,
            formats = new[] { "text", "docx", "xlsx", "pptx", "pdf" }, targetSearchableChars = TargetSearchableChars,
            opaquePaddingBytes = 0, utc = DateTime.UtcNow, seconds = watch.Elapsed.TotalSeconds
        }, Json));
        Console.WriteLine($"Generated mixed corpus {index} files / {total} bytes in {watch.Elapsed.TotalSeconds:F2}s");
    }

    private static string Content(int index, bool asciiOnly)
    {
        var builder = new StringBuilder(TargetSearchableChars + 1024);
        var random = new Random(unchecked(20260905 + index * 104729));
        int line = 0;
        while (builder.Length < TargetSearchableChars)
        {
            int id = unchecked(index * 1009 + line * 7919) & 0x7fffffff;
            builder.Append("PersonalRag configuration=value request_id=").Append(id)
                .Append(" request_").Append(id).Append(" ERROR timeout th in er 00 document validation ");
            if (!asciiOnly) builder.Append("日本語の文書検索 ");
            // Deterministic high-entropy searchable payload prevents compression-only padding from gaming the 5% ratio.
            for (int token = 0; token < 8; token++) builder.Append(random.NextInt64().ToString("x16"));
            builder.Append(" line_").Append(line).Append('\n');
            line++;
        }
        return builder.ToString();
    }

    private static void CreatePlain(string path, string text)
        => File.WriteAllText(path, text, new UTF8Encoding(false));

    private static void CreateDocx(string path, string text)
    {
        using var zip = ZipFile.Open(path, ZipArchiveMode.Create);
        Write(zip, "[Content_Types].xml", """<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>""");
        Write(zip, "_rels/.rels", """<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>""");
        Write(zip, "word/document.xml", $"<?xml version=\"1.0\" encoding=\"UTF-8\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:r><w:t xml:space=\"preserve\">{Xml(text)}</w:t></w:r></w:p></w:body></w:document>");
    }

    private static void CreateXlsx(string path, string text)
    {
        using var zip = ZipFile.Open(path, ZipArchiveMode.Create);
        Write(zip, "[Content_Types].xml", """<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/></Types>""");
        Write(zip, "_rels/.rels", """<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>""");
        Write(zip, "xl/workbook.xml", """<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="SearchSheet" sheetId="1" r:id="rId1"/></sheets></workbook>""");
        Write(zip, "xl/_rels/workbook.xml.rels", """<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>""");
        Write(zip, "xl/sharedStrings.xml", $"<?xml version=\"1.0\" encoding=\"UTF-8\"?><sst xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" count=\"1\" uniqueCount=\"1\"><si><t xml:space=\"preserve\">{Xml(text)}</t></si></sst>");
        Write(zip, "xl/worksheets/sheet1.xml", """<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="27"><c r="B27" t="s"><v>0</v></c></row></sheetData></worksheet>""");
    }

    private static void CreatePptx(string path, string text)
    {
        using var zip = ZipFile.Open(path, ZipArchiveMode.Create);
        Write(zip, "[Content_Types].xml", """<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/></Types>""");
        Write(zip, "_rels/.rels", """<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/></Relationships>""");
        Write(zip, "ppt/presentation.xml", """<?xml version="1.0"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>""");
        Write(zip, "ppt/_rels/presentation.xml.rels", """<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>""");
        Write(zip, "ppt/slides/slide1.xml", $"<?xml version=\"1.0\" encoding=\"UTF-8\"?><p:sld xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"><p:cSld><p:spTree><p:sp><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{Xml(text)}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>");
    }

    private static void CreatePdf(string path, string text)
    {
        var builder = new PdfDocumentBuilder(); var font = builder.AddStandard14Font(Standard14Font.Helvetica);
        var lines = text.Split('\n', StringSplitOptions.RemoveEmptyEntries);
        int offset = 0;
        while (offset < lines.Length)
        {
            var page = builder.AddPage(PageSize.A4); double y = 800;
            for (int i = 0; i < 35 && offset < lines.Length; i++, offset++, y -= 20)
                page.AddText(lines[offset], 8, new PdfPoint(20, y), font);
        }
        File.WriteAllBytes(path, builder.Build());
    }

    private static void Write(ZipArchive zip, string name, string text)
    {
        var entry = zip.CreateEntry(name, CompressionLevel.Fastest); using var stream = entry.Open(); using var writer = new StreamWriter(stream, new UTF8Encoding(false)); writer.Write(text);
    }
    private static string Xml(string value) => System.Security.SecurityElement.Escape(value) ?? "";
}
