using System.IO.Compression;
using System.Xml.Linq;

namespace Astra.Core;

internal sealed class OpenXmlTextExtractor : ITextExtractor
{
    private static readonly XNamespace W = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    private static readonly XNamespace S = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    private static readonly XNamespace R = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    private static readonly XNamespace PR = "http://schemas.openxmlformats.org/package/2006/relationships";
    private static readonly XNamespace A = "http://schemas.openxmlformats.org/drawingml/2006/main";

    public IEnumerable<TextUnit> Extract(string path, CancellationToken cancellationToken = default)
    {
        string extension = Path.GetExtension(path).ToLowerInvariant();
        return extension switch
        {
            ".docx" => ExtractDocx(path, cancellationToken),
            ".xlsx" => ExtractXlsx(path, cancellationToken),
            ".pptx" => ExtractPptx(path, cancellationToken),
            _ => throw new InvalidDataException("Unsupported Open XML document type")
        };
    }

    private static IEnumerable<TextUnit> ExtractDocx(string path, CancellationToken cancellationToken)
    {
        using var archive = ZipFile.OpenRead(path);
        var parts = archive.Entries
            .Where(e => e.FullName.Equals("word/document.xml", StringComparison.OrdinalIgnoreCase) ||
                        e.FullName.StartsWith("word/header", StringComparison.OrdinalIgnoreCase) ||
                        e.FullName.StartsWith("word/footer", StringComparison.OrdinalIgnoreCase) ||
                        e.FullName.Equals("word/footnotes.xml", StringComparison.OrdinalIgnoreCase) ||
                        e.FullName.Equals("word/endnotes.xml", StringComparison.OrdinalIgnoreCase))
            .OrderBy(e => e.FullName, StringComparer.OrdinalIgnoreCase)
            .ToArray();
        if (parts.Length == 0) throw new InvalidDataException("DOCX document.xml is missing");
        long paragraph = 0;
        foreach (var part in parts)
        {
            cancellationToken.ThrowIfCancellationRequested();
            var document = Load(part);
            foreach (var p in document.Descendants(W + "p"))
            {
                cancellationToken.ThrowIfCancellationRequested();
                string text = string.Concat(p.DescendantNodes().OfType<XElement>().Select(e =>
                    e.Name == W + "t" ? e.Value : e.Name == W + "tab" ? "\t" : e.Name == W + "br" ? "\n" : ""));
                if (text.Length == 0) continue;
                paragraph++;
                string prefix = part.FullName.Equals("word/document.xml", StringComparison.OrdinalIgnoreCase) ? "Paragraph" : PartLabel(part.FullName);
                yield return new TextUnit($"{prefix} {paragraph}", text);
            }
        }
    }

    private static IEnumerable<TextUnit> ExtractXlsx(string path, CancellationToken cancellationToken)
    {
        using var archive = ZipFile.OpenRead(path);
        var shared = ReadSharedStrings(archive, cancellationToken);
        var workbookEntry = Entry(archive, "xl/workbook.xml") ?? throw new InvalidDataException("XLSX workbook.xml is missing");
        var relEntry = Entry(archive, "xl/_rels/workbook.xml.rels") ?? throw new InvalidDataException("XLSX workbook relationships are missing");
        var workbook = Load(workbookEntry); var relationships = Load(relEntry);
        var targets = relationships.Root?.Elements(PR + "Relationship")
            .Where(e => e.Attribute("Id") is not null && e.Attribute("Target") is not null)
            .ToDictionary(e => (string)e.Attribute("Id")!, e => (string)e.Attribute("Target")!, StringComparer.Ordinal) ?? new Dictionary<string, string>(StringComparer.Ordinal);

        foreach (var sheet in workbook.Descendants(S + "sheet"))
        {
            cancellationToken.ThrowIfCancellationRequested();
            string name = (string?)sheet.Attribute("name") ?? "Sheet";
            string? relId = (string?)sheet.Attribute(R + "id");
            if (relId is null || !targets.TryGetValue(relId, out string? target)) continue;
            string partName = NormalizePart("xl", target);
            var sheetEntry = Entry(archive, partName); if (sheetEntry is null) continue;
            var sheetXml = Load(sheetEntry);
            foreach (var cell in sheetXml.Descendants(S + "c"))
            {
                cancellationToken.ThrowIfCancellationRequested();
                string location = (string?)cell.Attribute("r") ?? "?";
                string type = (string?)cell.Attribute("t") ?? "";
                string value = CellText(cell, type, shared);
                if (value.Length == 0) continue;
                yield return new TextUnit($"{name}!{location}", value);
            }
        }
    }

    private static IEnumerable<TextUnit> ExtractPptx(string path, CancellationToken cancellationToken)
    {
        using var archive = ZipFile.OpenRead(path);
        var slides = archive.Entries.Where(e => e.FullName.StartsWith("ppt/slides/slide", StringComparison.OrdinalIgnoreCase) &&
                                                e.FullName.EndsWith(".xml", StringComparison.OrdinalIgnoreCase))
            .OrderBy(e => SlideNumber(e.FullName)).ToArray();
        if (slides.Length == 0) throw new InvalidDataException("PPTX contains no slides");
        foreach (var entry in slides)
        {
            cancellationToken.ThrowIfCancellationRequested();
            var document = Load(entry);
            string text = string.Join("\n", document.Descendants(A + "p")
                .Select(p => string.Concat(p.Descendants(A + "t").Select(t => t.Value)))
                .Where(s => s.Length > 0));
            if (text.Length > 0) yield return new TextUnit($"Slide {SlideNumber(entry.FullName)}", text);
        }
    }

    private static List<string> ReadSharedStrings(ZipArchive archive, CancellationToken cancellationToken)
    {
        var entry = Entry(archive, "xl/sharedStrings.xml"); if (entry is null) return [];
        var document = Load(entry); var result = new List<string>();
        foreach (var item in document.Descendants(S + "si"))
        {
            cancellationToken.ThrowIfCancellationRequested();
            result.Add(string.Concat(item.Descendants(S + "t").Select(t => t.Value)));
        }
        return result;
    }

    private static string CellText(XElement cell, string type, List<string> shared)
    {
        if (type == "inlineStr") return string.Concat(cell.Descendants(S + "t").Select(t => t.Value));
        string value = cell.Element(S + "v")?.Value ?? "";
        if (type == "s" && int.TryParse(value, out int index) && index >= 0 && index < shared.Count) value = shared[index];
        string? formula = cell.Element(S + "f")?.Value;
        return formula is { Length: > 0 } ? $"={formula} {value}" : value;
    }

    private static XDocument Load(ZipArchiveEntry entry)
    {
        try { using var stream = entry.Open(); return XDocument.Load(stream, LoadOptions.PreserveWhitespace); }
        catch (Exception ex) when (ex is System.Xml.XmlException or InvalidOperationException or IOException)
        { throw new InvalidDataException($"Invalid Open XML part: {entry.FullName}", ex); }
    }

    private static ZipArchiveEntry? Entry(ZipArchive archive, string name)
        => archive.Entries.FirstOrDefault(e => e.FullName.Equals(name, StringComparison.OrdinalIgnoreCase));

    private static string NormalizePart(string baseDirectory, string target)
    {
        target = target.Replace('\\', '/');
        if (target.StartsWith('/')) return target.TrimStart('/');
        var stack = new List<string>(baseDirectory.Split('/', StringSplitOptions.RemoveEmptyEntries));
        foreach (string segment in target.Split('/', StringSplitOptions.RemoveEmptyEntries))
        {
            if (segment == ".") continue;
            if (segment == "..") { if (stack.Count > 0) stack.RemoveAt(stack.Count - 1); }
            else stack.Add(segment);
        }
        return string.Join('/', stack);
    }

    private static int SlideNumber(string path)
    {
        string name = Path.GetFileNameWithoutExtension(path);
        return int.TryParse(name.AsSpan("slide".Length), out int number) ? number : int.MaxValue;
    }

    private static string PartLabel(string fullName)
    {
        string name = Path.GetFileNameWithoutExtension(fullName);
        return name.StartsWith("header", StringComparison.OrdinalIgnoreCase) ? "Header" :
               name.StartsWith("footer", StringComparison.OrdinalIgnoreCase) ? "Footer" :
               name.Equals("footnotes", StringComparison.OrdinalIgnoreCase) ? "Footnote" : "Endnote";
    }
}
