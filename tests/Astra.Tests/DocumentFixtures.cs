using System.IO.Compression;
using System.Text;
using UglyToad.PdfPig.Content;
using UglyToad.PdfPig.Core;
using UglyToad.PdfPig.Fonts.Standard14Fonts;
using UglyToad.PdfPig.Writer;

internal static class DocumentFixtures
{
    public static void Create(string root)
    {
        Directory.CreateDirectory(root);
        CreateDocx(Path.Combine(root, "sample.docx"), "GateTwoDocxNeedle unique document text");
        CreateXlsx(Path.Combine(root, "sample.xlsx"));
        CreatePptx(Path.Combine(root, "sample.pptx"));
        CreatePdf(Path.Combine(root, "sample.pdf"));
    }

    public static void RewriteDocx(string root, string text)
    {
        string path = Path.Combine(root, "sample.docx");
        if (File.Exists(path)) File.Delete(path);
        CreateDocx(path, text);
    }

    private static void CreateDocx(string path, string text)
    {
        using var zip = ZipFile.Open(path, ZipArchiveMode.Create);
        Write(zip, "word/document.xml", $"""
            <?xml version="1.0" encoding="UTF-8" standalone="yes"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>{System.Security.SecurityElement.Escape(text)}</w:t></w:r></w:p></w:body>
            </w:document>
            """);
    }

    private static void CreateXlsx(string path)
    {
        using var zip = ZipFile.Open(path, ZipArchiveMode.Create);
        Write(zip, "xl/workbook.xml", """
            <?xml version="1.0" encoding="UTF-8"?>
            <workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
              <sheets><sheet name="SearchSheet" sheetId="1" r:id="rId1"/></sheets>
            </workbook>
            """);
        Write(zip, "xl/_rels/workbook.xml.rels", """
            <?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
            </Relationships>
            """);
        Write(zip, "xl/sharedStrings.xml", """
            <?xml version="1.0" encoding="UTF-8"?>
            <sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1">
              <si><t>GateTwoXlsxNeedle unique cell text</t></si>
            </sst>
            """);
        Write(zip, "xl/worksheets/sheet1.xml", """
            <?xml version="1.0" encoding="UTF-8"?>
            <worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
              <sheetData><row r="27"><c r="B27" t="s"><v>0</v></c></row></sheetData>
            </worksheet>
            """);
    }

    private static void CreatePptx(string path)
    {
        using var zip = ZipFile.Open(path, ZipArchiveMode.Create);
        Write(zip, "ppt/slides/slide1.xml", """
            <?xml version="1.0" encoding="UTF-8"?>
            <p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
              <p:cSld><p:spTree><p:sp><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>GateTwoPptxNeedle unique slide text</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld>
            </p:sld>
            """);
    }

    private static void CreatePdf(string path)
    {
        var builder = new PdfDocumentBuilder();
        var font = builder.AddStandard14Font(Standard14Font.Helvetica);
        var page = builder.AddPage(PageSize.A4);
        page.AddText("GateTwoPdfNeedle unique page text", 12, new PdfPoint(25, 700), font);
        File.WriteAllBytes(path, builder.Build());
    }

    private static void Write(ZipArchive zip, string name, string text)
    {
        var entry = zip.CreateEntry(name, CompressionLevel.Fastest);
        using var stream = entry.Open();
        using var writer = new StreamWriter(stream, new UTF8Encoding(false));
        writer.Write(text.TrimStart());
    }
}
