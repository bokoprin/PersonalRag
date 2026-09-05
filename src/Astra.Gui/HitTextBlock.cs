using System.Windows;
using System.Windows.Controls;
using System.Windows.Documents;
using System.Windows.Media;
using Astra.Core;

namespace Astra.Gui;

public sealed class HitTextBlock : TextBlock
{
    public static readonly DependencyProperty HitProperty = DependencyProperty.Register(nameof(Hit), typeof(Hit), typeof(HitTextBlock),
        new PropertyMetadata(null, (element, _) => ((HitTextBlock)element).RenderHit()));
    public Hit? Hit { get => (Hit?)GetValue(HitProperty); set => SetValue(HitProperty, value); }
    private void RenderHit()
    {
        Inlines.Clear();
        if (Hit is not { } hit) return;
        int start = Math.Clamp(hit.Start, 0, hit.Text.Length), length = Math.Clamp(hit.Length, 0, hit.Text.Length - start);
        Inlines.Add(new Run(hit.Text[..start]));
        Inlines.Add(new Run(hit.Text.Substring(start, length)) { Background = new SolidColorBrush(Color.FromRgb(255, 241, 168)) });
        Inlines.Add(new Run(hit.Text[(start + length)..]));
    }
}
