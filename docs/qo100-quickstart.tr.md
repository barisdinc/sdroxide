# QO-100'e çıkış — hızlı başlangıç (sdroxide + ADALM-Pluto)

**ADALM-Pluto** ve **Ku-band LNB** ile bir istasyonu QO-100 (Es'hail-2) sabit
yörüngeli transponderına sdroxide üzerinden çıkarmak için görev odaklı bir
anlatım. Bir kerelik istasyon ayarlarını, uyduya kilitlenmeyi ve QO-100
beacon senkronizasyon sekmesini kapsar. Her kontrolün ayrıntısı için
[`USER_MANUAL.md`](USER_MANUAL.md) §2.21 (QO-100 beacon plugin) ve §6.2.7
(PlutoSDR) bölümlerine bakın.

10 GHz downlink LNB üzerinden iner; 2,4 GHz uplink doğrudan Pluto'dan çıkar.

## Gerekenler

- **ADALM-Pluto** (veya LibreSDR / ANTSDR sınıfı bir AD936x kartı).
- Ara frekansı (LO) **9750 MHz** olan üniversal **Ku-band LNB** — QO-100'de
  kullandığımız standart.
- Çanak anten, LNB'den gelen koaks ve 2,4 GHz uplink anteni.
- Kurulu **sdroxide**. Pluto sürücüsü dahilidir; başka bir şey kurmaya gerek
  yoktur.

## Frekans zinciri

```
downlink   10489.750 MHz --> LNB (LO 9750 MHz) --> 739.750 MHz --> Pluto
uplink      2400.xxx MHz <-- Pluto (doğrudan, çevrilmez)
```

**Converter** gibi kalın yazılan adlar ekrandaki düğme ve alan adlarıdır.
`Settings > Radio` bir menü yolu; `192.168.2.1` ise yazacağınız bir değerdir.

---

## Bölüm A — Ayarlar

Bir kez yapılır ve saklanır. Hepsi **Settings** penceresinde, **General** ve
**Radio** sekmelerinde.

### 1. sdroxide'yi başlatın

Programı açın. Ana pencerede üstte kontrol çubuğu, altında panadapter ve şelale
bulunur.

### 2. Ayarları açın

Üst kontrol çubuğundaki **SETTINGS** düğmesine basın.

```
+-------------------------------------------------------------+
|  BAND   MODE   FILTER   AGC   [ SETTINGS ]   SAT   SCAN      |   <- 2
+-------------------------------------------------------------+
```

### 3. Çağrı işareti ve locator

**General** sekmesinde **Callsign** alanına çağrı işaretinizi, **Locator** alanına
Maidenhead karenizi (ör. `KN41GG`) girin. Uydu kilidi ve dünya haritası bu
locator'ı kullanır — girilmezse **LOCK ON** çalışmaz.

```
+- Settings ---------------------------------------------+
|  [ General ]  Radio   Servers   TLE                    |
|                                                       |
|   Callsign   [ TA1XYZ            ]        <- 3         |
|   Locator    [ KN41GG   ]                 <- 3         |
+-------------------------------------------------------+
```

### 4. Radio sekmesi — cihaz: PlutoSDR

**Radio** sekmesine geçin ve arayüz seçicisinden **PlutoSDR**'ı seçin. Pluto,
USB kablosunda bile bir ağ cihazıdır: kabloyu takınca seri port değil, bir ağ
adaptörü oluşur. Sonraki beş alan da bu sekmededir:

```
+- Settings --------------------------------------------------------+
|  General   [ Radio ]   Servers                                    |
|                                                                  |
|   Interface     [ PlutoSDR                        v ]     <- 4    |
|   Converter     [ LNB, Ku low (-9750 MHz)         v ]     <- 5    |
|   Offset        [ -9 750 000 000 Hz ]   (ön ayardan otomatik)     |
|   Transmit      [ Its own offset v ]   [ 0 ] Hz           <- 6    |
|   -------------------------------------------------------          |
|   Address       [ 192.168.2.1 ]        [ Discover ]       <- 7    |
|   Sample rate   [ 1 Msps v ]    [x] Full duplex           <- 8    |
|                                              [ Apply ]    <- 9    |
+------------------------------------------------------------------+
```

### 5. Konverter: LNB, Ku low (-9750 MHz)

**Converter** açılır menüsünden **LNB, Ku low (-9750 MHz)** ön ayarını seçin —
LNB'lerimizin ara frekansı 9750 MHz olduğu için. Bu, alttaki
`-9 750 000 000 Hz` offset'ini otomatik yazar; böylece 10489.750 MHz beacon
kadranda doğru yere yakın düşer.

### 6. Transmit satırı: Its own offset = 0

Konverter offset'inin yanındaki **Transmit** satırında **Its own offset** seçin
ve yanındaki Hz kutusuna `0` yazın.

Eskiden yaptığımız gibi TX için 8085 MHz benzeri bir değer yazmaya gerek yoktur.
Bu `0`, "downlink LNB'den geliyor ama 2,4 GHz uplink doğrudan radyodan çıkıyor —
verici tarafı çevrilmiyor" demektir. İlerde TX frekansınızın kayık olduğunu fark
ederseniz, bu kutudan düzeltirsiniz (Hz olarak, işaret kuralı alıcı offset'iyle
aynı).

### 7. Pluto adresi

Alttaki bağlantı bölümünde **Address** alanına Pluto'nun IP'sini yazın ya da
**Discover** ile buldurun. USB'den bağlıysanız `192.168.2.1` yeterlidir. Ağdan
erişiyorsanız Pluto'nun o ağdaki adresini yazın. `ip:192.168.2.1` de kabul
edilir; `usb:` ile başlayan adres kabul edilmez, çünkü bu arka uç radyoya USB
kablosunun sağladığı ağ üzerinden ulaşır.

### 8. Sample rate ve Full duplex

**Sample rate** değerini `1 Msps` seçin ve altındaki **Full duplex** kutusunu
işaretleyin; böylece verici açıkken bile kendi downlink'inizi duyarsınız.

> **Not.** **Full duplex** yalnızca gerçek Ethernet bağlantısında çalışır —
> gigabit adaptör arkasındaki bir Pluto ya da LibreSDR. USB üzerindeyseniz
> istasyon yarım çift yönlüdür ve over sırasında kendi downlink'inizi duymazsınız.
> Ayrıca stok bir Pluto yaklaşık 2,084 Msps altına inemez; daha düşük değer yukarı
> yuvarlanır ve bağlantı mesajı bunu söyler.

### 9. Apply ve kapat

En alttaki **Apply** düğmesine basın ve **Settings** penceresini kapatın.
Ayarlar Apply / yeniden bağlanma anında yürürlüğe girer.

---

## Bölüm B — Uyduya kilitlenme

Uydu çalışacağınız her seferde yapılır.

### 10. SAT penceresini açın

System box'taki **SAT** düğmesine basın. Kilit çalışırken düğme yeşil yanar;
pencere kapalı olsa bile düzeltme uygulanmaya devam eder.

```
+ System +        +- SAT ---------------------------+
| [ SAT ] |  -->  |  [ SATELLITES ]   [ QO-100 ]     |
|  SCAN   |  10   |  Kilit varken SAT yeşil yanar.   |
|  MEM    |       +---------------------------------+
+---------+
```

### 11. QO-100'ü seçin

**SATELLITES** sekmesinde arama kutusuna `QO-100` yazıp listeden seçin. Uydunun
yayınlanmış linkleri (dar bant transponder, beacon) altında belirir.

```
+- SAT - SATELLITES -------------------------------------+
|  arama: [ QO-100 ]                                     |
|  +-------------------------------------------------+   |
|  | > QO-100 (Es'hail-2)              geostationary  |  |  <- 11
|  |   ISS (ZARYA)                                    |  |
|  |   RS-44                                          |  |
|  +-------------------------------------------------+   |
|  NB transponder - beacon 10489.750 MHz                 |
|                            [ TUNE ]   [ LOCK ON ]      |  <- 12
+-------------------------------------------------------+
```

### 12. LOCK ON

Alttaki **LOCK ON** düğmesine basın. QO-100 sabit yörüngede olduğu için Doppler
düzeltmesi gerekmez; transponder haritası, downlink kadranını nereye ayarlarsanız
2,4 GHz uplink'i oradan türetir.

---

## Bölüm C — Beacon senkronizasyonu

Opsiyonel ama önerilir. LNB'niz çok kararlı değilse, ATÖLYE grubunun katkı
sağladığı QO-100 beacon senkronizasyon eklentisi, 10489.750 MHz dar bant
beacon'ının gerçekte nerede olduğunu ölçer, konverter/LNB offset'ini kadran ile
sinyal örtüşecek şekilde düzeltir ve LNB ısındıkça düzeltmeyi sürdürür.

### 13. QO-100 sekmesine geçin

SAT penceresinin 2. sekmesidir.

> **Ön koşul.** Eklenti birkaç kHz ila onlarca kHz'lik *artık* hatayı düzeltir —
> sıfırdan tahmin yapmaz. `Settings > Radio` konverter offset'inin önce yaklaşık
> doğru olması gerekir (adım 5), ki beacon 10489.750 MHz civarına düşsün.

```
+- SAT - QO-100 -----------------------------------------------------+
|  SATELLITES   [ QO-100 * ]                                         |
|                                                                   |
|  [ ON ]   [ TELEMETRY ]   [ AUTO ]        width [-] +/-25 kHz [+]  |
|    ^17         ^18           ^17                        ^14        |
|  +-----------------------------------------------------------+     |
|  |  mini şelale       ### = park lane                        |     |
|  |  : = 10489.750 MHz hedef       || || = beacon, iki lob    |     |
|  |  beacon'ın ortasına çift tıklayıp işaretleyin   <- 16     |     |
|  +-----------------------------------------------------------+     |
|  TRACKER            +1.2 kHz   (null 12 dB - snr 15 dB)            |
|  CONVERTER OFFSET   -9 749 920 000 Hz                             |
|  MEASURED           10489.751200 MHz     DRIFT +1.2 kHz            |
|                                                                   |
|                    [  APPLY CORRECTION  ]                 <- 16    |
+------------------------------------------------------------------+
```

### 14. Beacon'ı şeritte görün

Beacon bir bant / iki simetrik lob halinde görünmüyorsa, **width** değerini
**+** ile artırın (±5 kHz adımlarla, ±50 kHz'e kadar). İki lob ile aradaki boşluk
net görünene kadar genişletin, sonra tekrar ±5 kHz'e doğru daraltın.

### 15. Hâlâ görünmüyorsa: 9750'yi elle kaydırın

`Settings > Radio`'daki 9750 MHz konverter offset'ini uygun yönde yaklaşık bir
değere kaydırıp **Apply** deyin; beacon şeride girene kadar tekrarlayın. Gerisini
eklenti halleder.

### 16. Basit (tek seferlik) düzeltme

Şeritte beacon'ın ortasına **çift tıklayın**. Bu, "beacon burada" işaretini koyar
(iki lob, ortadaki boşlukta merkezlenir). Sonra alttaki **APPLY CORRECTION**
düğmesine basın. Alıcı düzeltilmiş offset ile yeniden açılır ve beacon merkeze
sıçrar. İki geçiş — biri geniş (kaba), biri dar (ince) — beacon'ı birkaç yüz Hz
içine sokar.

### 17. Otomatik, sürekli düzeltme

**ON** yapın (spektral izleyici başlar — yalnızca *ölçer*, hiçbir şeyi
değiştirmez), sonra **AUTO** seçin (döngü kapanır: her temiz, kararlı ölçüm
yavaş, hız sınırlı bir adımla offset'e uygulanır).

AUTO bundan sonra LNB kaymasını arka planda düzeltir. Tek bir gürültülü okuma
alıcıyı kaydırmaz; offset'in oynaması için birbirini tutan bir dizi ölçüm
gerekir. Siz verici açıkken düzeltme yapılmaz — her düzeltme alıcıyı yeniden
açar, bu da over'ınızı keserdi — bu yüzden bekleyen düzeltme over biter bitmez
uygulanır. AUTO kendi işini de denetler ve offset'i beacon'ı oynatmadan yazıyorsa
ya da hiçbir LNB'nin olamayacağı kadar uzağa gittiyse kendini kapatır ve nedenini
bildirir.

### 18. Telemetri (opsiyonel)

AO-40 çerçeve çözücülerini çalıştırmak için **TELEMETRY** düğmesine basın;
kilitlendiğinde beacon'ın kendi durum metni görünür. Kalibrasyon için gerekli
değildir — bağımsız bir kontroldür.

### 19. Arka planda çalışır bırakın

İşiniz bitince pencereyi küçültüp ekranın bir köşesinde tutun (kapatmak da olur).
LNB düzeltmesi pencere kapalıyken de sürer; **SAT** çipi yanık kalır ve QO-100
sekmesi kendi noktasını korur.

```
+- sdroxide - ana pencere ------------------------------+
|  panadapter / şelale              +- SAT - QO-100 * -+ |
|                                   | [ AUTO ]  son     | |
|                                   |  -80 Hz           | |
|                                   +-------------------+ |
|   paneli bir köşede açık bırakın - pencere kapalıyken  |
|   de düzeltmeyi sürdürür                               |
+------------------------------------------------------+
```

İyi QSO'lar, 73.
