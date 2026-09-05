using UglyToad.PdfPig;
using UglyToad.PdfPig.DocumentLayoutAnalysis.TextExtractor;

namespace Astra.Core;

internal sealed class PdfTextExtractor : ITextExtractor
{
    public IEnumerable<TextUnit> Extract(string path, CancellationToken cancellationToken = default)
    {
        PdfDocument document;
        try { document = PdfDocument.Open(path); }
        catch (Exception ex) when (ex is not OperationCanceledException and not OutOfMemoryException)
        { throw new InvalidDataException("PDF cannot be opened", ex); }
        return Enumerate(document, cancellationToken);
    }

    private static IEnumerable<TextUnit> Enumerate(PdfDocument document, CancellationToken cancellationToken)
    {
        using (document)
        {
            foreach (var page in document.GetPages())
            {
                cancellationToken.ThrowIfCancellationRequested();
                string text;
                try { text = ContentOrderTextExtractor.GetText(page); }
                catch (Exception ex) when (ex is not OperationCanceledException and not OutOfMemoryException)
                { throw new InvalidDataException($"PDF page {page.Number} cannot be extracted", ex); }
                if (!string.IsNullOrWhiteSpace(text)) yield return new TextUnit($"Page {page.Number}", text);
            }
        }
    }
}
