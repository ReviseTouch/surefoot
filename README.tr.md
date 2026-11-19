# surefoot

Neyin üstünde durduğunu soran, Rust için paralel bir dizin yürüyücüsü.

[English](README.md)

Yürüyücülerin çoğu bir iş parçacığı sayısı alır ve onu değiştirmez. Doğru sayı
diske bağlıdır: sekiz iş parçacığı NVMe için doğru, dönen disk için yanlıştır;
orada her fazladan okuyucu bir kafa hareketidir. surefoot önce yola sorar, sonra
yürürken bakmayı sürdürür.

```rust
use std::ops::ControlFlow;
use surefoot::Walk;

let report = Walk::new(["/home"]).for_each(false, |entry| {
    if let Ok(entry) = entry {
        println!("{}", entry.path.display());
    }
    ControlFlow::Continue(())
});
eprintln!("{} girdi, iş parçacıkları {:?}", report.seen, report.threads);
```

## Ne yapar

- **Ortamı sorar.** `surefoot::probe` bir yolun SSD'de mi, dönen diskte mi, ağ
  bağlamasında mı, bellekte mi olduğunu; hangi fiziksel diski başka yollarla
  paylaştığını; dosya sisteminin adını; isimlerin büyük-küçük harfe duyarlı
  olup olmadığını ve dosya kiplerinin gerçek olup olmadığını söyler. Linux,
  Windows ve macOS.
- **Hızını kendi ayarlar.** `Threads::Auto` ile yürüyüş ortamın değdiği sayıdan
  başlar, deneme olarak bir iş parçacığı ekler ve onu yalnız verim bir iş
  parçacığının katması gerekenin makul bir payı kadar arttıysa tutar. Okuyan iş
  parçacıkları zamanlarını tüketiciyi bekleyerek geçiriyorsa birini geri verir.
  Dönen disk tek iş parçacığında kalır.
- **Kullanılmayanın bedelini ödemez.** Ziyaretçi bir isim ve bir tür görür,
  `stat`'ı kendisi ister; geri çevirdiği girdi hiçbir şeye mal olmaz. Linux'ta
  `stat` tam yol yerine açık dizin üzerinden yapılır; Windows'ta dizin
  listesi onu zaten taşır.
- **Sınırlıdır.** Ziyaretçinin aldıkları çağırana sınırlı bir kuyruk üzerinden
  partiler hâlinde ulaşır. Yavaş tüketici yürüyüşü yavaşlatır, belleği büyütmez.
- **Okuyamadığını bildirir.** Her ret, yolu ve `io::Error`'ı ile gelir; yürüyüş
  etrafından devam eder.
- **Her karar ezilebilir.** `Threads::Fixed(n)`, `.medium(…)`, `.priority(…)`,
  `.batch(…)`, `.queue(…)`.

## Ölçüm

Tek makine, tek ağaç: NVMe üzerinde btrfs'te 5,6 milyon girdi, önbellek sıcak,
değişkenler dönüşümlü çalıştırıldı, üç turun ortancası.

| | 2 iş parçacığı | 8 iş parçacığı |
|---|---|---|
| `ignore` 0.4, tam yolla `stat` | 3,73 s | 1,18 s |
| surefoot, açık dizin üzerinden `stat` | 2,92 s | 0,98 s |
| ikisi de, `stat` yok | 1,4–1,6 s | 0,57 s |

`Threads::Auto` aynı ağacı iki iş parçacığıyla başlayıp altıya çıkarak 1,5 s'de
yürüdü. Bu kadar kısa bir yürüyüşte sabit sekizi geçmez; diski bilmeyen çağıran
içindir.

Ölçülmedi: soğuk önbellek, dönen disk, ağ bağlaması, Windows, macOS.

## Durum

0.1, [Scour](https://github.com/ReviseTouch/Scour) tarafından kullanılıyor. API
değişebilir.

- `Threads::Auto`, tüketicinin diskten yavaş olduğu yerde henüz kazanç
  değil. Sınırı indeksleyicisi olan Scour'un içinde sabit iki iş parçacığına
  kaybetti (11,9 s'ye karşı 14,0 s), çünkü tüketicinin kullanamayacağı iş
  parçacıklarını denemeyi sürdürdü. Orada `Threads::Fixed` kullanın.
- Hız ayarlayıcı kendi verimini ve tüketicisini görür. Başka süreçleri görmez;
  onlara yol vermek `Priority::Background`'ın işidir ve yalnız Linux'ta vardır.
- macOS taşınabilir yolu kullanır. `getattrlistbulk` henüz kullanılmıyor.
- NTFS biriminin ana dosya tablosundan (MFT) okunması henüz burada değil.

## Lisans

MIT ya da Apache-2.0, seçiminize göre.
